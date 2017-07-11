extern crate getopts;
extern crate rand;

use getopts::Options;
use std::env;
use std::thread;
use std::sync::mpsc::channel;
use std::collections::HashMap;
use std::net::UdpSocket;
use std::time::Duration;

fn print_usage(program: &str, opts: Options) {
    let program_path = std::path::PathBuf::from(program);
    let program_name = program_path.file_stem().unwrap().to_str().unwrap();
    let brief = format!("Usage: {} [-b BIND_ADDR] -l LOCAL_PORT -h REMOTE_ADDR -r REMOTE_PORT",
                        program_name);
    print!("{}", opts.usage(&brief));
}

fn main() {
    let args: Vec<String> = env::args().collect();
    let program = args[0].clone();

    let mut opts = Options::new();
    opts.reqopt("l",
                "local-port",
                "The local port to which udpproxy should bind to",
                "LOCAL_PORT");
    opts.reqopt("r",
                "remote-port",
                "The remote port to which UDP packets should be forwarded",
                "REMOTE_PORT");
    opts.reqopt("h",
                "host",
                "The remote address to which packets will be forwarded",
                "REMOTE_ADDR");
    opts.optopt("b",
                "bind",
                "The address on which to listen for incoming requests",
                "BIND_ADDR");

    let matches = opts.parse(&args[1..])
        .unwrap_or_else(|_| {
                            print_usage(&program, opts);
                            std::process::exit(-1);
                        });

    let local_port: i32 = matches.opt_str("l").unwrap().parse().unwrap();
    let remote_port: i32 = matches.opt_str("r").unwrap().parse().unwrap();
    let remote_host = matches.opt_str("h").unwrap();
    let bind_addr = match matches.opt_str("b") {
        Some(addr) => addr,
        None => "127.0.0.1".to_owned(),
    };

    forward(&bind_addr, local_port, &remote_host, remote_port);
}

fn forward(bind_addr: &str, local_port: i32, remote_host: &str, remote_port: i32) {
    let local_addr = format!("{}:{}", bind_addr, local_port);
    let local = UdpSocket::bind(&local_addr).expect(&format!("Unable to bind to {}", &local_addr));
    println!("Listening on {}", local.local_addr().unwrap());

    let remote_addr = format!("{}:{}", remote_host, remote_port);

    let responder = local
        .try_clone()
        .expect(&format!("Failed to clone primary listening address socket {}",
                        local.local_addr().unwrap()));
    let (main_sender, main_receiver) = channel::<(_, Vec<u8>)>();
    thread::spawn(move || {
        println!("Started new thread to deal out responses to clients");
        loop {
            let (dest, buf) = main_receiver.recv().unwrap();
            let to_send = buf.as_slice();
            responder
                .send_to(to_send, dest)
                .expect(&format!("Failed to forward response from upstream server to client {}",
                                dest));
        }
    });

    let mut client_map = HashMap::new();
    let mut buf = [0; 64 * 1024];
    loop {
        let (num_bytes, src_addr) = local.recv_from(&mut buf).expect("Didn't receive data");

        //we create a new thread for each unique client
        let mut remove_existing = false;
        loop {
            println!("Received packet from client {}", src_addr);

            let mut ignore_failure = true;
            let client_id = format!("{}", src_addr);
            let sender;
            {
                if remove_existing {
                    println!("Removing existing forwarder from map.");
                    client_map.remove(&client_id);
                }
                sender = client_map.entry(client_id.clone()).or_insert_with(|| {
                    //we are creating a new listener now, so a failure to send shoud be treated as an error
                    ignore_failure = false;

                    let local_send_queue = main_sender.clone();
                    let (sender, receiver) = channel::<Vec<u8>>();
                    let remote_addr_copy = remote_addr.clone();
                    thread::spawn(move|| {
                        let temp_outgoing_addr = format!("0.0.0.0:{}", 1024 + rand::random::<u16>());
                        println!("Establishing new forwarder for client {} on {}", src_addr, &temp_outgoing_addr);
                        let upstream_send = UdpSocket::bind(&temp_outgoing_addr)
                            .expect(&format!("Failed to bind to transient address {}", &temp_outgoing_addr));
                        let upstream_recv = upstream_send.try_clone()
                            .expect("Failed to clone client-specific connection to upstream!");

                        use std::sync::Arc;
                        use std::sync::atomic::{AtomicBool, Ordering};
                        let mut timeouts : u64 = 0;
                        let dealer_closed = Arc::<AtomicBool>::new(AtomicBool::new(false));
                        const TIMEOUT_UNIT: u64 = 1;

                        let local_dealer_closed = dealer_closed.clone();
                        thread::spawn(move|| {
                            let mut from_upstream = [0; 64 * 1024];
                            upstream_recv.set_read_timeout(Some(Duration::from_millis(TIMEOUT_UNIT + 100))).unwrap();
                            loop {
                                match upstream_recv.recv_from(&mut from_upstream) {
                                    Ok((bytes_rcvd, _)) => {
                                        let to_send = from_upstream[..bytes_rcvd].to_vec();
                                        local_send_queue.send((src_addr, to_send))
                                            .expect("Failed to queue response from upstream server for forwarding!");
                                    },
                                    Err(_) => {
                                        if local_dealer_closed.load(Ordering::Relaxed) {
                                            println!("Terminating forwarder threader for client {} due to timeout", src_addr);
                                            break;
                                        }
                                    }
                                };
                            }
                        });

                        loop {
                            match receiver.recv_timeout(Duration::from_millis(TIMEOUT_UNIT)) {
                                Ok(from_client) =>  {
                                    upstream_send.send_to(from_client.as_slice(), &remote_addr_copy)
                                        .expect(&format!("Failed to forward packet from client {} to upstream server!", src_addr));
                                    timeouts = 0; //reset timeout count
                                },
                                Err(_) => {
                                    timeouts += 1;
                                    if timeouts >= 10 {
                                            println!("Disconnecting forwarder for client {} due to timeout", src_addr);
                                            dealer_closed.store(true, Ordering::Relaxed);
                                        break;
                                    }
                                }
                            };
                        }
                    });
                    sender
                });
            }
            let to_send = buf[..num_bytes].to_vec();
            //sender.send(to_send).expect(&format!("Failed to queue received datagram from client {} for sending to upstream server!", &src_addr));
            match sender.send(to_send) {
                Ok(_) => {
                    break;
                }
                Err(_) => {
                    if !ignore_failure {
                        panic!("Failed to send message to datagram forwarder for client {}",
                               client_id);
                    }
                    //client previously timed out
                    println!("New connection received from previously timed-out client {}",
                             client_id);
                    remove_existing = true;
                    continue;
                }
            }
        }
    }
}
