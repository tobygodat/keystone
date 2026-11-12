//! A project's dev servers: loopback TCP listeners whose process works inside the
//! project folder and that answer `GET /` with a page or a redirect.

use std::collections::{BTreeMap, HashMap};
use std::io::{Read, Write};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

const CRLF: &str = "\r\n";

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DevServer {
    pub port: u16,
    pub url: String,
    /// The listening process's command name, such as `node`.
    pub process: String,
}

/// A listening socket and its process, read from `lsof -F pcn`.
#[derive(Clone, Debug, Eq, PartialEq)]
struct Listener {
    pid: u32,
    command: String,
    address: IpAddr,
    port: u16,
}

/// The dev servers of `directory`, by port. Runs `lsof` and connects to each candidate,
/// so call it off the UI thread.
pub fn find(directory: &Path) -> Vec<DevServer> {
    let Ok(directory) = directory.canonicalize() else {
        return Vec::new();
    };
    let Some(output) = run(&["-nP", "-iTCP", "-sTCP:LISTEN", "-Fpcn"]) else {
        return Vec::new();
    };
    let listeners = parse_listeners(&output);
    let pids: Vec<String> = {
        let mut pids: Vec<u32> = listeners.iter().map(|l| l.pid).collect();
        pids.sort_unstable();
        pids.dedup();
        pids.iter().map(u32::to_string).collect()
    };
    if pids.is_empty() {
        return Vec::new();
    }
    let cwds = run(&["-a", "-d", "cwd", "-Fpn", "-p", &pids.join(",")])
        .map(|output| parse_cwds(&output))
        .unwrap_or_default();
    let mut found = BTreeMap::new();
    for listener in listeners {
        let inside = cwds
            .get(&listener.pid)
            .is_some_and(|cwd| cwd.starts_with(&directory));
        if inside
            && !found.contains_key(&listener.port)
            && serves_pages(listener.address, listener.port)
        {
            found.insert(
                listener.port,
                DevServer {
                    port: listener.port,
                    url: format!("http://localhost:{}", listener.port),
                    process: listener.command,
                },
            );
        }
    }
    found.into_values().collect()
}

fn run(args: &[&str]) -> Option<String> {
    let output = Command::new("lsof").args(args).output().ok()?;
    String::from_utf8(output.stdout).ok()
}

/// Loopback and wildcard listeners from `lsof -Fpcn` output.
fn parse_listeners(output: &str) -> Vec<Listener> {
    let mut listeners = Vec::new();
    let (mut pid, mut command) = (None, String::new());
    for line in output.lines() {
        let (field, value) = line.split_at(line.len().min(1));
        match field {
            "p" => {
                pid = value.parse().ok();
                command.clear();
            }
            "c" => command = value.to_owned(),
            "n" => {
                let Some((host, port)) = value.rsplit_once(':') else {
                    continue;
                };
                let (Some(pid), Ok(port)) = (pid, port.parse::<u16>()) else {
                    continue;
                };
                let address = match host.trim_start_matches('[').trim_end_matches(']') {
                    "*" | "0.0.0.0" => IpAddr::V4(Ipv4Addr::LOCALHOST),
                    "::" => IpAddr::V6(Ipv6Addr::LOCALHOST),
                    "localhost" => IpAddr::V4(Ipv4Addr::LOCALHOST),
                    other => match other.parse::<IpAddr>() {
                        Ok(address) if address.is_loopback() => address,
                        _ => continue,
                    },
                };
                listeners.push(Listener {
                    pid,
                    command: command.clone(),
                    address,
                    port,
                });
            }
            _ => {}
        }
    }
    listeners
}

/// Each process's working directory from `lsof -a -d cwd -Fpn` output.
fn parse_cwds(output: &str) -> HashMap<u32, PathBuf> {
    let mut cwds = HashMap::new();
    let mut pid = None;
    for line in output.lines() {
        let (field, value) = line.split_at(line.len().min(1));
        match field {
            "p" => pid = value.parse().ok(),
            "n" => {
                if let Some(pid) = pid {
                    cwds.insert(pid, PathBuf::from(value));
                }
            }
            _ => {}
        }
    }
    cwds
}

/// Whether `GET /` answers with HTML or a redirect within a second.
fn serves_pages(address: IpAddr, port: u16) -> bool {
    let timeout = Duration::from_millis(500);
    let Ok(mut stream) = TcpStream::connect_timeout(&SocketAddr::new(address, port), timeout)
    else {
        return false;
    };
    let _ = stream.set_read_timeout(Some(timeout));
    let _ = stream.set_write_timeout(Some(timeout));
    let request =
        format!("GET / HTTP/1.0{CRLF}Host: localhost:{port}{CRLF}Accept: text/html{CRLF}{CRLF}");
    if stream.write_all(request.as_bytes()).is_err() {
        return false;
    }
    let mut head = Vec::new();
    let _ = stream.take(8192).read_to_end(&mut head);
    is_page(&String::from_utf8_lossy(&head))
}

fn is_page(head: &str) -> bool {
    let mut lines = head.lines();
    let Some(status) = lines
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse::<u16>().ok())
    else {
        return false;
    };
    let html = lines
        .take_while(|line| !line.is_empty())
        .filter_map(|line| line.split_once(':'))
        .any(|(name, value)| {
            name.eq_ignore_ascii_case("content-type")
                && value.to_ascii_lowercase().contains("text/html")
        });
    (300..400).contains(&status) || (200..300).contains(&status) && html
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn listeners_keep_loopback_and_wildcard_addresses() {
        let output = "p501\ncnode\nn127.0.0.1:5173\nn[::1]:5174\nn*:3000\np777\ncpython3\nn192.168.1.4:8000\nn[::]:8001\n";
        let found = parse_listeners(output);
        let ports: Vec<(u32, &str, u16)> = found
            .iter()
            .map(|l| (l.pid, l.command.as_str(), l.port))
            .collect();
        assert_eq!(
            ports,
            vec![
                (501, "node", 5173),
                (501, "node", 5174),
                (501, "node", 3000),
                (777, "python3", 8001)
            ]
        );
        assert_eq!(found[1].address, IpAddr::V6(Ipv6Addr::LOCALHOST));
    }

    #[test]
    fn working_directories_are_read_per_process() {
        let cwds = parse_cwds("p501\nfcwd\nn/Users/me/app\np777\nfcwd\nn/tmp\n");
        assert_eq!(cwds[&501], PathBuf::from("/Users/me/app"));
        assert_eq!(cwds[&777], PathBuf::from("/tmp"));
    }

    #[test]
    fn pages_and_redirects_count_but_apis_do_not() {
        assert!(is_page(
            "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\n\r\n<html>"
        ));
        assert!(is_page("HTTP/1.1 302 Found\r\nLocation: /login\r\n\r\n"));
        assert!(!is_page(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\r\n{}"
        ));
        assert!(!is_page(
            "HTTP/1.1 404 Not Found\r\nContent-Type: text/html\r\n\r\n"
        ));
        assert!(!is_page("SSH-2.0-OpenSSH_9.6\r\n"));
    }

    #[test]
    fn a_page_server_in_the_folder_is_found() {
        let folder = tempfile::tempdir().unwrap();
        let listener = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let mut request = [0; 1024];
                let _ = stream.read(&mut request);
                let _ = stream
                    .write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/html\r\n\r\n<html></html>");
            }
        });
        assert!(serves_pages(IpAddr::V4(Ipv4Addr::LOCALHOST), port));
        server.join().unwrap();
        assert!(
            find(folder.path()).is_empty(),
            "no process works in a fresh folder"
        );
    }
}
