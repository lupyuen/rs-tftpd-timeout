// Testing:
// clear && cargo build && sudo cargo run -- -i 0.0.0.0 -p 69 -d "$HOME/tftproot"
// curl -v --output initrd tftp://192.168.x.x/initrd

use crate::{Packet, Socket, Window};
use std::{
    error::Error,
    fs::{self, File},
    path::PathBuf,
    thread,
    time::{Duration, Instant},
};

const MAX_RETRIES: u32 = 6;
const TIMEOUT_BUFFER: Duration = Duration::from_secs(1);

/// Worker `struct` is used for multithreaded file sending and receiving.
/// It creates a new socket using the Server's IP and a random port
/// requested from the OS to communicate with the requesting client.
///
/// See [`Worker::send()`] and [`Worker::receive()`] for more details.
///
/// # Example
///
/// ```rust
/// use std::{net::{UdpSocket, SocketAddr}, path::PathBuf, str::FromStr, time::Duration};
/// use tftpd::Worker;
///
/// // Send a file, responding to a read request.
/// let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
/// socket.connect(SocketAddr::from_str("127.0.0.1:12345").unwrap()).unwrap();
///
/// let worker = Worker::new(
///     Box::new(socket),
///     PathBuf::from_str("Cargo.toml").unwrap(),
///     512,
///     Duration::from_secs(1),
///     1,
/// );
///
/// worker.send().unwrap();
/// ```
pub struct Worker<T: Socket + ?Sized> {
    socket: Box<T>,
    file_name: PathBuf,
    blk_size: usize,
    timeout: Duration,
    windowsize: u16,
}

impl<T: Socket + ?Sized> Worker<T> {
    /// Creates a new [`Worker`] with the supplied options.
    pub fn new(
        socket: Box<T>,
        file_name: PathBuf,
        blk_size: usize,
        timeout: Duration,
        windowsize: u16,
    ) -> Worker<T> {
        Worker {
            socket,
            file_name,
            blk_size,
            timeout,
            windowsize,
        }
    }

    /// Sends a file to the remote [`SocketAddr`] that has sent a read request using
    /// a random port, asynchronously.
    pub fn send(self) -> Result<(), Box<dyn Error>> {
        let file_name = self.file_name.clone();
        let remote_addr = self.socket.remote_addr().unwrap();

        thread::spawn(move || {
            let handle_send = || -> Result<(), Box<dyn Error>> {
                self.send_file(File::open(&file_name)?)?;

                Ok(())
            };

            match handle_send() {
                Ok(_) => {
                    println!(
                        "Sent {} to {}",
                        &file_name.file_name().unwrap().to_string_lossy(),
                        &remote_addr
                    );
                }
                Err(err) => {
                    eprintln!("{err}");
                }
            }
        });

        Ok(())
    }

    /// Receives a file from the remote [`SocketAddr`] that has sent a write request using
    /// the supplied socket, asynchronously.
    pub fn receive(self) -> Result<(), Box<dyn Error>> {
        let file_name = self.file_name.clone();
        let remote_addr = self.socket.remote_addr().unwrap();

        thread::spawn(move || {
            let handle_receive = || -> Result<(), Box<dyn Error>> {
                self.receive_file(File::create(&file_name)?)?;

                Ok(())
            };

            match handle_receive() {
                Ok(_) => {
                    println!(
                        "Received {} from {}",
                        &file_name.file_name().unwrap().to_string_lossy(),
                        remote_addr
                    );
                }
                Err(err) => {
                    eprintln!("{err}");
                    if fs::remove_file(&file_name).is_err() {
                        eprintln!("Error while cleaning {}", &file_name.to_str().unwrap());
                    }
                }
            }
        });

        Ok(())
    }

    fn send_file(self, file: File) -> Result<(), Box<dyn Error>> {
        let mut block_number = 1;
        let mut window = Window::new(self.windowsize, self.blk_size, file);

        loop {
            let filled = window.fill()?;

            let mut retry_cnt = 0;
            let mut time = Instant::now() - (self.timeout + TIMEOUT_BUFFER);
            loop {
                if time.elapsed() >= self.timeout {
                    send_window(&self.socket, &window, block_number)?;
                    time = Instant::now();
                }

                match self.socket.recv() {
                    Ok(Packet::Ack(received_block_number)) => {
                        let diff = received_block_number.wrapping_sub(block_number);
                        if diff <= self.windowsize {
                            block_number = received_block_number.wrapping_add(1);
                            window.remove(diff + 1)?;
                            break;
                        }
                    }
                    Ok(Packet::Error { code, msg }) => {
                        return Err(format!("Received error code {code}: {msg}").into());
                    }
                    _ => {
                        retry_cnt += 1;
                        if retry_cnt == MAX_RETRIES {
                            return Err(
                                format!("Transfer timed out after {MAX_RETRIES} tries").into()
                            );
                        }
                    }
                }
            }

            if !filled && window.is_empty() {
                break;
            }
        }

        Ok(())
    }

    fn receive_file(self, file: File) -> Result<(), Box<dyn Error>> {
        let mut block_number: u16 = 0;
        let mut window = Window::new(self.windowsize, self.blk_size, file);

        loop {
            let mut size;
            let mut retry_cnt = 0;

            loop {
                match self.socket.recv_with_size(self.blk_size) {
                    Ok(Packet::Data {
                        block_num: received_block_number,
                        data,
                    }) => {
                        if received_block_number == block_number.wrapping_add(1) {
                            block_number = received_block_number;
                            size = data.len();
                            window.add(data)?;

                            if size < self.blk_size {
                                break;
                            }

                            if window.is_full() {
                                break;
                            }
                        }
                    }
                    Ok(Packet::Error { code, msg }) => {
                        return Err(format!("Received error code {code}: {msg}").into());
                    }
                    _ => {
                        retry_cnt += 1;
                        if retry_cnt == MAX_RETRIES {
                            return Err(
                                format!("Transfer timed out after {MAX_RETRIES} tries").into()
                            );
                        }
                    }
                }
            }

            window.empty()?;
            self.socket.send(&Packet::Ack(block_number))?;
            if size < self.blk_size {
                break;
            };
        }

        Ok(())
    }
}

fn send_window<T: Socket>(
    socket: &T,
    window: &Window,
    mut block_num: u16,
) -> Result<(), Box<dyn Error>> {
    // println!("send_window: block_num={}", block_num);////
    for frame in window.get_elements() {
        socket.send(&Packet::Data {
            block_num,
            data: frame.to_vec(),
        })?;

        unsafe {
            // Wait a while before sending next block
            // println!("send_window loop: block_num={}", block_num);////
            static mut DELAY_MS: u64 = 1;
            let millis = std::time::Duration::from_millis(DELAY_MS);
            std::thread::sleep(millis);

            // Check whether this is a resend
            static mut LAST_BLOCK_NUM: u16 = 0;            
            if block_num > 1 && block_num <= LAST_BLOCK_NUM {
                println!("*** send_window RESEND: block_num={}", block_num);
                DELAY_MS = DELAY_MS * 2;
            }

            // Check whether this is a delayed send
            static mut LAST_TIMESTAMP: once_cell::sync::Lazy::<std::time::Instant> = 
                once_cell::sync::Lazy::new(|| std::time::Instant::now());
            let diff_time = std::time::Instant::now() - *LAST_TIMESTAMP;
            if block_num > 1 && diff_time > Duration::from_millis(1000) {
                println!("+++ send_window DELAY: block_num={}", block_num);
                DELAY_MS = DELAY_MS * 2;
            }
            LAST_BLOCK_NUM = block_num;
            *LAST_TIMESTAMP = std::time::Instant::now();
        }

        block_num = block_num.wrapping_add(1);
    }

    Ok(())
}

/* Output Log
Sending Image to 192.168.31.141:3900
+++ send_window DELAY: block_num=676
Sent Image to 192.168.31.141:3900
Sending jh7110-star64-pine64.dtb to 192.168.31.141:2434
Sent jh7110-star64-pine64.dtb to 192.168.31.141:2434
Sending initrd to 192.168.31.141:2539
+++ send_window DELAY: block_num=15
+++ send_window DELAY: block_num=2366
+++ send_window DELAY: block_num=2755
+++ send_window DELAY: block_num=5012
Sent initrd to 192.168.31.141:2539
Sending Image to 192.168.31.141:4069
+++ send_window DELAY: block_num=795
Sending jh7110-star64-pine64.dtb to 192.168.31.141:2647
Sent Image to 192.168.31.141:4069
Sent jh7110-star64-pine64.dtb to 192.168.31.141:2647
Sending initrd to 192.168.31.141:1859
+++ send_window DELAY: block_num=61
+++ send_window DELAY: block_num=1711
*/
