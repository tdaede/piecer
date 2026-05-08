use clap::{Parser, Subcommand};
use rusb::*;
use std::time::Duration;
use std::str;
use std::fs;
use std::fs::File;
use std::io::Write;
use std::thread::sleep;

const TIMEOUT: Duration = Duration::from_secs(1);
const SECTOR_SIZE: usize = 2*1024;
const FIRMWARE_START: usize = 8*1024;
const FIRMWARE_SIZE: usize = 512*1024 - FIRMWARE_START;
const FLASH_ADDR: u32 = 0xc00000;

struct DirEnt {
    name: String,
    cluster: u16,
    len: u32
}

struct Piece {
    device_handle: DeviceHandle<GlobalContext>,
    pffs_top: u32,
}

enum AppStatus {
    Running = 1,
    Stopped = 3,
}


#[derive(Parser)]
#[command()]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// List all files on device
    Ls,
    /// Display a screenshot in terminal
    Screenshot,
    /// Download a single file to current directory
    Download {
        file: String,
    },
    /// Dump flash to dump.img
    Dump,
    /// Download all files to current directory
    Backup,
    /// Load a file at a specific memory location
    LoadFile {
        file: String,
        addr: u32,
    },
    /// Run an SRF file
    RunSRF {
        file: String,
    },
    /// Write a new firmware image
    FlashFirmware {
        updater_srf: String,
        file: String,
    },
}

impl Piece {
    fn new() -> Piece {
        let device_handle = open_device_with_vid_pid(0x0e19, 0x1000).expect("Could not open PIECE device");
        device_handle.claim_interface(0).unwrap();
        device_handle.write_bulk(0x02, &[0, 32], TIMEOUT).unwrap();
        let mut version = [0; 32];
        device_handle.read_bulk(0x82, &mut version, TIMEOUT).unwrap();
        let pffs_top = u32::from_le_bytes(version[24..28].try_into().unwrap());
        return Piece { device_handle, pffs_top }
    }
    fn get_memory(&mut self, addr: u32, len: u32, data: &mut [u8]) {
        let mut bytes_left = len;
        loop {
            let bytes_to_read = bytes_left.min(32);
            let mut command: Vec<u8> = vec![2];
            command.extend((addr+len-bytes_left).to_le_bytes());
            command.extend(bytes_to_read.to_le_bytes());
            self.device_handle.write_bulk(0x02, &command, TIMEOUT).unwrap();
            self.device_handle.read_bulk(0x82, &mut data[((len-bytes_left) as usize)..], TIMEOUT).unwrap();
            bytes_left -= bytes_to_read;
            if bytes_left == 0 {
                break;
            }
        }
    }
    fn put_memory(&mut self, addr: u32, data: &[u8]) {
        let len = data.len() as u32;
        let mut bytes_left = len;
        while bytes_left != 0 {
            let offset = len - bytes_left;
            let bytes_to_write = bytes_left.min(32);
            let mut command: Vec<u8> = vec![3];
            command.extend((addr+offset).to_le_bytes());
            command.extend(bytes_to_write.to_le_bytes());
            self.device_handle.write_bulk(0x02, &command, TIMEOUT).unwrap();
            let payload = &data[(offset as usize)..(offset+bytes_to_write) as usize];
            self.device_handle.write_bulk(0x02, payload, TIMEOUT).unwrap();
            bytes_left -= bytes_to_write;
        }
    }
    fn erase_sector(&mut self, addr: u32) {
        assert!(addr >= 0xC02000 && addr < 0xC80000);
        let mut command: Vec<u8> = vec![8];
        command.extend(addr.to_le_bytes());
        println!("erase: {command:02x?}");
        self.device_handle.write_bulk(0x02, &command, TIMEOUT).unwrap();
        let mut resp: [u8; 2] = [0; 2];
        self.device_handle.read_bulk(0x82, &mut resp, TIMEOUT).unwrap();
        assert!(u16::from_le_bytes(resp) == 0);
    }
    fn write_sector(&mut self, addr: u32, data: &[u8; SECTOR_SIZE]) {
        assert!(addr >= 0xC02000 && addr < 0xC80000);
        // transfer the sector into RAM
        let mut xfer_cmd: Vec<u8> = vec![3];
        const SEC_BUFFER: u32 = 0x102C00;
        xfer_cmd.extend(SEC_BUFFER.to_le_bytes());
        xfer_cmd.extend((data.len() as u32).to_le_bytes());
        self.device_handle.write_bulk(0x02, &xfer_cmd, TIMEOUT).unwrap();
        self.device_handle.write_bulk(0x02, data, TIMEOUT).unwrap();
        // copy RAM to flash
        let mut flash_cmd: Vec<u8> = vec![9];
        flash_cmd.extend(addr.to_le_bytes());
        flash_cmd.extend(SEC_BUFFER.to_le_bytes());
        flash_cmd.extend((data.len() as u32).to_le_bytes());
        println!("flash: {flash_cmd:?}");
        self.device_handle.write_bulk(0x02, &flash_cmd, TIMEOUT).unwrap();
        let mut resp: [u8; 2] = [0; 2];
        self.device_handle.read_bulk(0x82, &mut resp, TIMEOUT).unwrap();
        assert!(u16::from_le_bytes(resp) == 0);
    }
    fn reset(&mut self) {
        // get first word of vector table
        const VECTOR_BASE: u32 = 0xC00000;
        let mut resp: [u8; 4] = [0; 4];
        self.get_memory(VECTOR_BASE, 4, &mut resp);
        let reset_addr = u32::from_le_bytes(resp);
        println!("P/ECE will reset from: 0x{:04X}", reset_addr);
        self.execute(reset_addr);
    }
    fn execute(&mut self, addr: u32) {
        let mut cmd: Vec<u8> = vec![1];
        cmd.extend(addr.to_le_bytes());
        self.device_handle.write_bulk(0x02, &cmd, TIMEOUT).unwrap();
    }
    fn pause(&mut self) {
        self.device_handle.write_bulk(0x02, &[16, 1], TIMEOUT).unwrap();
    }
    fn resume(&mut self) {
        self.device_handle.write_bulk(0x02, &[16, 0], TIMEOUT).unwrap();
    }
    fn set_app_status(&mut self, status: AppStatus) {
        let (cmd, resp): ([u8;2], u16) = match status {
            AppStatus::Stopped => ([4, AppStatus::Stopped as u8], 0),
            AppStatus::Running => ([4, AppStatus::Running as u8], 2),
        };
        self.device_handle.write_bulk(0x02, &cmd, TIMEOUT).unwrap();
        println!("sleeping while we wait for app status to change");
        sleep(Duration::from_secs(5));
        assert!(self.get_app_status() == resp)
    }
    fn get_app_status(&mut self) -> u16 {
        self.device_handle.write_bulk(0x02, &[5], TIMEOUT).unwrap();
        let mut resp = [0; 2];
        self.device_handle.read_bulk(0x82, &mut resp, TIMEOUT).unwrap();
        return u16::from_le_bytes(resp);
    }
    fn get_screenshot(&mut self) {
        self.pause();
        self.device_handle.write_bulk(0x02, &[17], TIMEOUT).unwrap();
        let mut lcd_data = [0; 12];
        self.device_handle.read_bulk(0x82, &mut lcd_data, TIMEOUT).unwrap();
        println!("LCD data: {:?}", lcd_data);
        let lcd_width = lcd_data[2];
        let lcd_height = lcd_data[4];
        assert_eq!(lcd_width, 128);
        assert_eq!(lcd_height, 88);
        let lcd_addr = u32::from_le_bytes(lcd_data[8..12].try_into().unwrap());
        for y in 0..88 {
            let mut line = [0; 128];
            self.get_memory(lcd_addr + y * 128, 128, &mut line);
            for p in line {
                print!("{}", match p {
                    3 => " ",
                    2 => "░",
                    1 => "▒",
                    0 => "▓",
                    _ => "X"
                });
            }
            println!();
        }
        self.resume();
    }
    fn ls(&mut self) -> Vec<DirEnt> {
        let mut directory = Vec::<DirEnt>::new();
        for i in 1..96 {
            let mut dirent_raw = [0; 32];
            self.get_memory(self.pffs_top + i * 32, 32, &mut dirent_raw);
            if dirent_raw[0] != 0x00 && dirent_raw[0] != 0xFF {
                let dirent = DirEnt { name: str::from_utf8(&dirent_raw[0..24]).unwrap().trim_matches(char::from(0)).to_string(),
                                      cluster: u16::from_le_bytes(dirent_raw[26..28].try_into().unwrap()),
                                      len: u32::from_le_bytes(dirent_raw[28..32].try_into().unwrap()
                )};
                directory.push(dirent);
            }
        }
        directory
    }
    fn download(&mut self, filename: &str) {
        let mut clusters_raw = [0; 496*2];
        self.get_memory(self.pffs_top + 97 * 32, 496*2, &mut clusters_raw);
        let directory = self.ls();
        let dirent = directory.into_iter().find(|dirent| {
            dirent.name == filename
        }).expect("Could not find file to download");
        let mut file = File::create(dirent.name).unwrap();
        let mut cluster = dirent.cluster;
        let mut data_left = dirent.len as usize;
        loop {
            let mut data = [0; 4096];
            self.get_memory(self.pffs_top + 97 * 32 + 496 * 2 + (cluster as u32) * 4096 - 4096, 4096, &mut data);
            file.write_all(&data[..data_left.min(4096)]).unwrap();
            data_left -= data_left.min(4096);
            cluster = u16::from_le_bytes(clusters_raw[(cluster as usize)*2..(cluster as usize)*2+2].try_into().unwrap());
            if cluster > 0x8000 {
                break;
            }
        }
    }
    fn flash_firmware(&mut self, updater: &str, firmware: &str) {
        self.set_app_status(AppStatus::Stopped);
        self.load_file(firmware, 0x102C00);
        self.load_srf(updater);
        self.set_app_status(AppStatus::Running);
    }
    fn load_file(&mut self, filename: &str, addr: u32) {
        self.put_memory(addr, &fs::read(filename).unwrap());
    }
    fn load_srf(&mut self, filename: &str) {
        let srf = fs::read(filename).unwrap();
        // check for magic number
        assert!(u16::from_be_bytes(srf[0..2].try_into().unwrap())|8 == 0xE);
        let mut p = 8;
        loop {
            p = u32::from_be_bytes(srf[p..p+4].try_into().unwrap()) as usize;
            if p == 0 {
                break;
            }
            let len = u32::from_be_bytes(srf[p+38..p+42].try_into().unwrap());
            if len != 0 {
                let pos = u32::from_be_bytes(srf[p+34..p+38].try_into().unwrap());
                if pos != 0 {
                    let addr = u32::from_be_bytes(srf[p+10..p+14].try_into().unwrap());
                    self.put_memory(addr, &srf[(pos as usize)..((pos+len) as usize)]);
                }
            }
        }
    }
    fn run_srf(&mut self, filename: &str) {
        self.set_app_status(AppStatus::Stopped);
        self.load_srf(filename);
        self.set_app_status(AppStatus::Running);
    }
}

fn main() {
    let cli = Cli::parse();
    let mut piece = Piece::new();
    match cli.command {
        Commands::Ls => {
            for dirent in piece.ls() {
                println!("{}\t{}", dirent.name, dirent.len);
            }
        }
        Commands::Screenshot => {
            piece.get_screenshot();
        }
        Commands::Download {file} => {
            piece.download(file.as_str());
        }
        Commands::Dump => {
            let mut file = File::create("dump.img").expect("Could not create dump.img");
            let mut dump = [0; 2097152];
            piece.get_memory(0xc00000, 2097152, &mut dump);
            file.write_all(&dump).unwrap();
        }
        Commands::Backup => {
            for dirent in piece.ls() {
                println!("{}", dirent.name);
                piece.download(&dirent.name);
            }
        },
        Commands::LoadFile {file, addr} => {
            piece.load_file(file.as_str(), addr);
        },
        Commands::RunSRF {file} => {
            piece.run_srf(file.as_str());
        },
        Commands::FlashFirmware {updater_srf, file} => {
            piece.flash_firmware(updater_srf.as_str(), file.as_str());
        },
    }
}
