use clap::{Parser, Subcommand};
use rusb::*;
use std::time::Duration;
use std::str;
use std::fs::File;
use std::io::Write;

const TIMEOUT: Duration = Duration::from_secs(1);

struct DirEnt {
    name: String,
    cluster: u16,
    len: u32
}

struct Piece {
    device_handle: DeviceHandle<GlobalContext>,
    pffs_top: u32
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
    fn pause(&mut self) {
        self.device_handle.write_bulk(0x02, &[16, 1], TIMEOUT).unwrap();
    }
    fn resume(&mut self) {
        self.device_handle.write_bulk(0x02, &[16, 0], TIMEOUT).unwrap();
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
            piece.get_memory(0xc00000, 2097142, &mut dump);
            file.write_all(&dump).unwrap();
        }
        Commands::Backup => {
            for dirent in piece.ls() {
                println!("{}", dirent.name);
                piece.download(&dirent.name);
            }
        }
    }
}
