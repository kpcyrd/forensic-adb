/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

pub mod adb;
pub mod shell;

#[cfg(test)]
pub mod test;

use log::{debug, trace, warn};
use once_cell::sync::Lazy;
use regex::Regex;
use std::collections::BTreeMap;
use std::convert::TryFrom;
use std::io;
use std::iter::FromIterator;
use std::num::{ParseIntError, TryFromIntError};
use std::path::{Component, Path};
use std::str::{FromStr, Utf8Error};
use std::time::SystemTime;
use thiserror::Error;
use tokio::fs::File;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;
pub use unix_path::{Path as UnixPath, PathBuf as UnixPathBuf};
use uuid::Uuid;
use walkdir::WalkDir;

use crate::adb::{DeviceSerial, SyncCommand};

pub type Result<T> = std::result::Result<T, DeviceError>;

static SYNC_REGEX: Lazy<Regex> = Lazy::new(|| Regex::new(r"[^A-Za-z0-9_@%+=:,./-]").unwrap());

#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub enum AndroidStorageInput {
    #[default]
    Auto,
    App,
    Internal,
    Sdcard,
}

impl FromStr for AndroidStorageInput {
    type Err = DeviceError;

    fn from_str(s: &str) -> Result<Self> {
        match s {
            "auto" => Ok(AndroidStorageInput::Auto),
            "app" => Ok(AndroidStorageInput::App),
            "internal" => Ok(AndroidStorageInput::Internal),
            "sdcard" => Ok(AndroidStorageInput::Sdcard),
            _ => Err(DeviceError::InvalidStorage),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum AndroidStorage {
    App,
    Internal,
    Sdcard,
}

#[derive(Debug, Error)]
pub enum DeviceError {
    #[error("{0}")]
    Adb(String),
    #[error(transparent)]
    FromInt(#[from] TryFromIntError),
    #[error("Invalid storage")]
    InvalidStorage,
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error("Missing package")]
    MissingPackage,
    #[error("Multiple Android devices online")]
    MultipleDevices,
    #[error(transparent)]
    ParseInt(#[from] ParseIntError),
    #[error("Unknown Android device with serial '{0}'")]
    UnknownDevice(String),
    #[error(transparent)]
    Utf8(#[from] Utf8Error),
    #[error(transparent)]
    WalkDir(#[from] walkdir::Error),
}

fn encode_message(payload: &str) -> Result<String> {
    let hex_length = u16::try_from(payload.len()).map(|len| format!("{:0>4X}", len))?;

    Ok(format!("{}{}", hex_length, payload))
}

fn parse_device_info(line: &str) -> Option<DeviceInfo> {
    // Turn "serial\tdevice key1:value1 key2:value2 ..." into a `DeviceInfo`.
    let mut pairs = line.split_whitespace();
    let serial = pairs.next();
    let state = pairs.next();
    if let (Some(serial), Some("device")) = (serial, state) {
        let info: BTreeMap<String, String> = pairs
            .filter_map(|pair| {
                let mut kv = pair.split(':');
                if let (Some(k), Some(v), None) = (kv.next(), kv.next(), kv.next()) {
                    Some((k.to_owned(), v.to_owned()))
                } else {
                    None
                }
            })
            .collect();

        Some(DeviceInfo {
            serial: serial.to_owned(),
            info,
        })
    } else {
        None
    }
}

/// Reads the payload length of a host message from the stream.
async fn read_length<R: AsyncRead + Unpin>(stream: &mut R) -> Result<usize> {
    let mut bytes: [u8; 4] = [0; 4];
    stream.read_exact(&mut bytes).await?;

    let response = std::str::from_utf8(&bytes)?;

    Ok(usize::from_str_radix(response, 16)?)
}

/// Reads the payload length of a device message from the stream.
async fn read_length_little_endian<R: AsyncRead + Unpin>(reader: &mut R) -> Result<usize> {
    let mut bytes: [u8; 4] = [0; 4];
    reader.read_exact(&mut bytes).await?;

    let n: usize = (bytes[0] as usize)
        + ((bytes[1] as usize) << 8)
        + ((bytes[2] as usize) << 16)
        + ((bytes[3] as usize) << 24);

    Ok(n)
}

/// Writes the payload length of a device message to the stream.
async fn write_length_little_endian<W: AsyncWrite + Unpin>(
    writer: &mut W,
    n: usize,
) -> Result<usize> {
    let mut bytes = [0; 4];
    bytes[0] = (n & 0xFF) as u8;
    bytes[1] = ((n >> 8) & 0xFF) as u8;
    bytes[2] = ((n >> 16) & 0xFF) as u8;
    bytes[3] = ((n >> 24) & 0xFF) as u8;

    writer.write(&bytes[..]).await.map_err(DeviceError::Io)
}

async fn read_response(
    stream: &mut TcpStream,
    has_output: bool,
    has_length: bool,
) -> Result<Vec<u8>> {
    let mut bytes: [u8; 1024] = [0; 1024];

    stream.read_exact(&mut bytes[0..4]).await?;

    if !bytes.starts_with(SyncCommand::Okay.code()) {
        let n = bytes.len().min(read_length(stream).await?);
        stream.read_exact(&mut bytes[0..n]).await?;

        let message = std::str::from_utf8(&bytes[0..n]).map(|s| format!("adb error: {}", s))?;

        return Err(DeviceError::Adb(message));
    }

    let mut response = Vec::new();

    if has_output {
        stream.read_to_end(&mut response).await?;

        if response.starts_with(SyncCommand::Okay.code()) {
            // Sometimes the server produces OKAYOKAY.  Sometimes there is a transport OKAY and
            // then the underlying command OKAY.  This is straight from `chromedriver`.
            response = response.split_off(4);
        }

        if response.starts_with(SyncCommand::Fail.code()) {
            // The server may even produce OKAYFAIL, which means the underlying
            // command failed. First split-off the `FAIL` and length of the message.
            response = response.split_off(8);

            let message = std::str::from_utf8(&response).map(|s| format!("adb error: {}", s))?;

            return Err(DeviceError::Adb(message));
        }

        if has_length {
            if response.len() >= 4 {
                let message = response.split_off(4);
                let slice: &mut &[u8] = &mut &*response;

                let n = read_length(slice).await?;
                if n != message.len() {
                    warn!("adb server response contained hexstring len {} but remaining message length is {}", n, message.len());
                }

                trace!(
                    "adb server response was {:?}",
                    std::str::from_utf8(&message)?
                );

                return Ok(message);
            } else {
                return Err(DeviceError::Adb(format!(
                    "adb server response did not contain expected hexstring length: {:?}",
                    std::str::from_utf8(&response)?
                )));
            }
        }
    }

    Ok(response)
}

/// Detailed information about an ADB device.
#[derive(Debug, Clone, Eq, PartialEq, Ord, PartialOrd)]
pub struct DeviceInfo {
    pub serial: DeviceSerial,
    pub info: BTreeMap<String, String>,
}

/// Represents a connection to an ADB host, which multiplexes the connections to
/// individual devices.
#[derive(Debug, Clone, PartialEq)]
pub struct Host {
    /// The TCP host to connect to.  Defaults to `"localhost"`.
    pub host: Option<String>,
    /// The TCP port to connect to.  Defaults to `5037`.
    pub port: Option<u16>,
}

impl Default for Host {
    fn default() -> Host {
        Host {
            host: Some("localhost".to_string()),
            port: Some(5037),
        }
    }
}

impl Host {
    /// Searches for available devices, and selects the one as specified by `device_serial`.
    ///
    /// If multiple devices are online, and no device has been specified,
    /// the `ANDROID_SERIAL` environment variable can be used to select one.
    pub async fn device_or_default<T: AsRef<str>>(
        self,
        device_serial: Option<&T>,
        storage: AndroidStorageInput,
    ) -> Result<Device> {
        let serials: Vec<String> = self
            .devices::<Vec<_>>()
            .await?
            .into_iter()
            .map(|d| d.serial)
            .collect();

        if let Some(ref serial) = device_serial
            .map(|v| v.as_ref().to_owned())
            .or_else(|| std::env::var("ANDROID_SERIAL").ok())
        {
            if !serials.contains(serial) {
                return Err(DeviceError::UnknownDevice(serial.clone()));
            }

            return Device::new(self, serial.to_owned(), storage).await;
        }

        if serials.len() > 1 {
            return Err(DeviceError::MultipleDevices);
        }

        if let Some(ref serial) = serials.first() {
            return Device::new(self, serial.to_owned().to_string(), storage).await;
        }

        Err(DeviceError::Adb("No Android devices are online".to_owned()))
    }

    pub async fn connect(&self) -> Result<TcpStream> {
        let stream = TcpStream::connect(format!(
            "{}:{}",
            self.host.clone().unwrap_or_else(|| "localhost".to_owned()),
            self.port.unwrap_or(5037)
        ))
        .await?;
        Ok(stream)
    }

    pub async fn execute_command(
        &self,
        command: &str,
        has_output: bool,
        has_length: bool,
    ) -> Result<String> {
        let mut stream = self.connect().await?;

        stream
            .write_all(encode_message(command)?.as_bytes())
            .await?;
        let bytes = read_response(&mut stream, has_output, has_length).await?;
        // TODO: should we assert no bytes were read?

        let response = std::str::from_utf8(&bytes)?;

        Ok(response.to_owned())
    }

    pub async fn execute_host_command(
        &self,
        host_command: &str,
        has_length: bool,
        has_output: bool,
    ) -> Result<String> {
        self.execute_command(&format!("host:{}", host_command), has_output, has_length)
            .await
    }

    pub async fn features<B: FromIterator<String>>(&self) -> Result<B> {
        let features = self.execute_host_command("features", true, true).await?;
        Ok(features.split(',').map(|x| x.to_owned()).collect())
    }

    pub async fn devices<B: FromIterator<DeviceInfo>>(&self) -> Result<B> {
        let response = self.execute_host_command("devices-l", true, true).await?;

        let infos: B = response.lines().filter_map(parse_device_info).collect();

        Ok(infos)
    }
}

/// Represents an ADB device.
#[derive(Debug, Clone)]
pub struct Device {
    /// ADB host that controls this device.
    pub host: Host,

    /// Serial number uniquely identifying this ADB device.
    pub serial: DeviceSerial,

    pub run_as_package: Option<String>,

    pub storage: AndroidStorage,

    /// Cache intermediate tempfile name used in pushing via run_as.
    pub tempfile: UnixPathBuf,
}

#[derive(Debug, Eq, PartialEq, Ord, PartialOrd)]
pub struct RemoteDirEntry {
    depth: usize,
    metadata: RemoteMetadata,
    name: String,
}

#[derive(Debug, Eq, PartialEq, Ord, PartialOrd)]
pub enum RemoteMetadata {
    RemoteFile(RemoteFileMetadata),
    RemoteDir,
    RemoteSymlink,
}
#[derive(Debug, Eq, PartialEq, Ord, PartialOrd)]
pub struct RemoteFileMetadata {
    mode: usize,
    size: usize,
}

impl Device {
    pub async fn new(
        host: Host,
        serial: DeviceSerial,
        storage: AndroidStorageInput,
    ) -> Result<Device> {
        let mut device = Device {
            host,
            serial,
            run_as_package: None,
            storage: AndroidStorage::App,
            tempfile: UnixPathBuf::from("/data/local/tmp"),
        };
        device
            .tempfile
            .push(Uuid::new_v4().as_hyphenated().to_string());

        device.storage = match storage {
            AndroidStorageInput::App => AndroidStorage::App,
            AndroidStorageInput::Internal => AndroidStorage::Internal,
            AndroidStorageInput::Sdcard => AndroidStorage::Sdcard,
            AndroidStorageInput::Auto => AndroidStorage::Sdcard,
        };

        Ok(device)
    }

    pub async fn clear_app_data(&self, package: &str) -> Result<bool> {
        self.execute_host_shell_command(&format!("pm clear {}", package))
            .await
            .map(|v| v.contains("Success"))
    }

    pub async fn create_dir(&self, path: &UnixPath) -> Result<()> {
        debug!("Creating {}", path.display());

        let enable_run_as = self.enable_run_as_for_path(path);
        self.execute_host_shell_command_as(&format!("mkdir -p {}", path.display()), enable_run_as)
            .await?;

        Ok(())
    }

    pub async fn chmod(&self, path: &UnixPath, mask: &str, recursive: bool) -> Result<()> {
        let enable_run_as = self.enable_run_as_for_path(path);

        let recursive = match recursive {
            true => " -R",
            false => "",
        };

        self.execute_host_shell_command_as(
            &format!("chmod {} {} {}", recursive, mask, path.display()),
            enable_run_as,
        )
        .await?;

        Ok(())
    }

    pub async fn execute_host_command(
        &self,
        command: &str,
        has_output: bool,
        has_length: bool,
    ) -> Result<Vec<u8>> {
        let mut stream = self.host.connect().await?;

        let switch_command = format!("host:transport:{}", self.serial);
        trace!("execute_host_command: >> {:?}", &switch_command);
        stream
            .write_all(encode_message(&switch_command)?.as_bytes())
            .await?;
        let _bytes = read_response(&mut stream, false, false).await?;
        trace!("execute_host_command: << {:?}", _bytes);
        // TODO: should we assert no bytes were read?

        trace!("execute_host_command: >> {:?}", &command);
        stream
            .write_all(encode_message(command)?.as_bytes())
            .await?;
        let bytes = read_response(&mut stream, has_output, has_length).await?;
        trace!("execute_host_command: << {:?}", bstr::BStr::new(&bytes));

        Ok(bytes)
    }

    pub async fn execute_host_command_to_string(
        &self,
        command: &str,
        has_output: bool,
        has_length: bool,
    ) -> Result<String> {
        let bytes = self
            .execute_host_command(command, has_output, has_length)
            .await?;

        let response = std::str::from_utf8(&bytes)?;

        // Unify new lines by removing possible carriage returns
        Ok(response.replace("\r\n", "\n"))
    }

    pub fn enable_run_as_for_path(&self, path: &UnixPath) -> bool {
        match &self.run_as_package {
            Some(package) => {
                let mut p = UnixPathBuf::from("/data/data/");
                p.push(package);
                path.starts_with(p)
            }
            None => false,
        }
    }

    pub async fn execute_host_shell_command(&self, shell_command: &str) -> Result<String> {
        self.execute_host_shell_command_as(shell_command, false)
            .await
    }

    pub async fn execute_host_exec_out_command(&self, shell_command: &str) -> Result<Vec<u8>> {
        self.execute_host_command(&format!("exec:{}", shell_command), true, false)
            .await
    }

    pub async fn execute_host_shell_command_as(
        &self,
        shell_command: &str,
        enable_run_as: bool,
    ) -> Result<String> {
        // We don't want to duplicate su invocations.
        if shell_command.starts_with("su") {
            return self
                .execute_host_command_to_string(&format!("shell:{}", shell_command), true, false)
                .await;
        }

        let has_outer_quotes = shell_command.starts_with('"') && shell_command.ends_with('"')
            || shell_command.starts_with('\'') && shell_command.ends_with('\'');

        // Execute command as package
        if enable_run_as {
            let run_as_package = self
                .run_as_package
                .as_ref()
                .ok_or(DeviceError::MissingPackage)?;

            if has_outer_quotes {
                return self
                    .execute_host_command_to_string(
                        &format!("shell:run-as {} {}", run_as_package, shell_command),
                        true,
                        false,
                    )
                    .await;
            }

            if SYNC_REGEX.is_match(shell_command) {
                let arg: &str = &shell_command.replace('\'', "'\"'\"'")[..];
                return self
                    .execute_host_command_to_string(
                        &format!("shell:run-as {} {}", run_as_package, arg),
                        true,
                        false,
                    )
                    .await;
            }

            return self
                .execute_host_command_to_string(
                    &format!("shell:run-as {} \"{}\"", run_as_package, shell_command),
                    true,
                    false,
                )
                .await;
        }

        self.execute_host_command_to_string(&format!("shell:{}", shell_command), true, false)
            .await
    }

    pub async fn is_app_installed(&self, package: &str) -> Result<bool> {
        self.execute_host_shell_command(&format!("pm path {}", package))
            .await
            .map(|v| v.contains("package:"))
    }

    pub async fn launch<T: AsRef<str>>(
        &self,
        package: &str,
        activity: &str,
        am_start_args: &[T],
    ) -> Result<bool> {
        let mut am_start = format!("am start -W -n {}/{}", package, activity);

        for arg in am_start_args {
            am_start.push(' ');
            if SYNC_REGEX.is_match(arg.as_ref()) {
                am_start.push_str(&format!("\"{}\"", &shell::escape(arg.as_ref())));
            } else {
                am_start.push_str(&shell::escape(arg.as_ref()));
            };
        }

        self.execute_host_shell_command(&am_start)
            .await
            .map(|v| v.contains("Complete"))
    }

    pub async fn force_stop(&self, package: &str) -> Result<()> {
        debug!("Force stopping Android package: {}", package);
        self.execute_host_shell_command(&format!("am force-stop {}", package))
            .await
            .and(Ok(()))
    }

    pub async fn forward_port(&self, local: u16, remote: u16) -> Result<u16> {
        let command = format!(
            "host-serial:{}:forward:tcp:{};tcp:{}",
            self.serial, local, remote
        );
        let response = self.host.execute_command(&command, true, false).await?;

        if local == 0 {
            Ok(response.parse::<u16>()?)
        } else {
            Ok(local)
        }
    }

    pub async fn kill_forward_port(&self, local: u16) -> Result<()> {
        let command = format!("host-serial:{}:killforward:tcp:{}", self.serial, local);
        self.execute_host_command(&command, true, false)
            .await
            .and(Ok(()))
    }

    pub async fn kill_forward_all_ports(&self) -> Result<()> {
        let command = format!("host-serial:{}:killforward-all", self.serial);
        self.execute_host_command(&command, false, false)
            .await
            .and(Ok(()))
    }

    pub async fn reverse_port(&self, remote: u16, local: u16) -> Result<u16> {
        let command = format!("reverse:forward:tcp:{};tcp:{}", remote, local);
        let response = self
            .execute_host_command_to_string(&command, true, false)
            .await?;

        if remote == 0 {
            Ok(response.parse::<u16>()?)
        } else {
            Ok(remote)
        }
    }

    pub async fn kill_reverse_port(&self, remote: u16) -> Result<()> {
        let command = format!("reverse:killforward:tcp:{}", remote);
        self.execute_host_command(&command, true, true)
            .await
            .and(Ok(()))
    }

    pub async fn kill_reverse_all_ports(&self) -> Result<()> {
        let command = "reverse:killforward-all".to_owned();
        self.execute_host_command(&command, false, false)
            .await
            .and(Ok(()))
    }

    pub async fn list_dir(&self, src: &UnixPath) -> Result<Vec<RemoteDirEntry>> {
        let src = src.to_path_buf();
        let mut queue = vec![(src.clone(), 0, "".to_string())];

        let mut listings = Vec::new();

        while let Some((next, depth, prefix)) = queue.pop() {
            for listing in self.list_dir_flat(&next, depth, prefix).await? {
                if listing.metadata == RemoteMetadata::RemoteDir {
                    let mut child = src.clone();
                    child.push(listing.name.clone());
                    queue.push((child, depth + 1, listing.name.clone()));
                }

                listings.push(listing);
            }
        }

        Ok(listings)
    }

    async fn list_dir_flat(
        &self,
        src: &UnixPath,
        depth: usize,
        prefix: String,
    ) -> Result<Vec<RemoteDirEntry>> {
        // Implement the ADB protocol to list a directory from the device.
        let mut stream = self.host.connect().await?;

        // Send "host:transport" command with device serial
        let message = encode_message(&format!("host:transport:{}", self.serial))?;
        stream.write_all(message.as_bytes()).await?;
        let _bytes = read_response(&mut stream, false, true).await?;

        // Send "sync:" command to initialize file transfer
        let message = encode_message("sync:")?;
        stream.write_all(message.as_bytes()).await?;
        let _bytes = read_response(&mut stream, false, true).await?;

        // Send "LIST" command with name of the directory
        stream.write_all(SyncCommand::List.code()).await?;
        let args_ = format!("{}", src.display());
        let args = args_.as_bytes();
        write_length_little_endian(&mut stream, args.len()).await?;
        stream.write_all(args).await?;

        // Use the maximum 64KB buffer to transfer the file contents.
        let mut buf = [0; 64 * 1024];

        let mut listings = Vec::new();

        // Read "DENT" command one or more times for the directory entries
        loop {
            stream.read_exact(&mut buf[0..4]).await?;

            if &buf[0..4] == SyncCommand::Dent.code() {
                // From https://github.com/cstyan/adbDocumentation/blob/6d025b3e4af41be6f93d37f516a8ac7913688623/README.md:
                //
                // A four-byte integer representing file mode - first 9 bits of this mode represent
                // the file permissions, as with chmod mode. Bits 14 to 16 seem to represent the
                // file type, one of 0b100 (file), 0b010 (directory), 0b101 (symlink)
                // A four-byte integer representing file size.
                // A four-byte integer representing last modified time in seconds since Unix Epoch.
                // A four-byte integer representing file name length.
                // A utf-8 string representing the file name.
                let mode = read_length_little_endian(&mut stream).await?;
                let size = read_length_little_endian(&mut stream).await?;
                let _time = read_length_little_endian(&mut stream).await?;
                let name_length = read_length_little_endian(&mut stream).await?;
                stream.read_exact(&mut buf[0..name_length]).await?;

                let mut name = std::str::from_utf8(&buf[0..name_length])?.to_owned();

                if name == "." || name == ".." {
                    continue;
                }

                if !prefix.is_empty() {
                    name = format!("{}/{}", prefix, &name);
                }

                let file_type = (mode >> 13) & 0b111;
                let metadata = match file_type {
                    0b010 => RemoteMetadata::RemoteDir,
                    0b100 => RemoteMetadata::RemoteFile(RemoteFileMetadata {
                        mode: mode & 0b111111111,
                        size,
                    }),
                    0b101 => RemoteMetadata::RemoteSymlink,
                    _ => return Err(DeviceError::Adb(format!("Invalid file mode {}", file_type))),
                };

                listings.push(RemoteDirEntry {
                    name,
                    depth,
                    metadata,
                });
            } else if &buf[0..4] == SyncCommand::Done.code() {
                // "DONE" command indicates end of file transfer
                break;
            } else if &buf[0..4] == SyncCommand::Fail.code() {
                let n = buf.len().min(read_length_little_endian(&mut stream).await?);

                stream.read_exact(&mut buf[0..n]).await?;

                let message = std::str::from_utf8(&buf[0..n])
                    .map(|s| format!("adb error: {}", s))
                    .unwrap_or_else(|_| "adb error was not utf-8".into());

                return Err(DeviceError::Adb(message));
            } else {
                return Err(DeviceError::Adb("FAIL (unknown)".to_owned()));
            }
        }

        Ok(listings)
    }

    pub async fn path_exists(&self, path: &UnixPath, enable_run_as: bool) -> Result<bool> {
        self.execute_host_shell_command_as(format!("ls {}", path.display()).as_str(), enable_run_as)
            .await
            .map(|path| !path.contains("No such file or directory"))
    }

    pub async fn pull<W: AsyncWrite + Unpin>(&self, src: &UnixPath, buffer: &mut W) -> Result<()> {
        // Implement the ADB protocol to receive a file from the device.
        let mut stream = self.host.connect().await?;

        // Send "host:transport" command with device serial
        let message = encode_message(&format!("host:transport:{}", self.serial))?;
        stream.write_all(message.as_bytes()).await?;
        let _bytes = read_response(&mut stream, false, true).await?;

        // Send "sync:" command to initialize file transfer
        let message = encode_message("sync:")?;
        stream.write_all(message.as_bytes()).await?;
        let _bytes = read_response(&mut stream, false, true).await?;

        // Send "RECV" command with name of the file
        stream.write_all(SyncCommand::Recv.code()).await?;
        let args_string = format!("{}", src.display());
        let args = args_string.as_bytes();
        write_length_little_endian(&mut stream, args.len()).await?;
        stream.write_all(args).await?;

        // Use the maximum 64KB buffer to transfer the file contents.
        let mut buf = [0; 64 * 1024];

        // Read "DATA" command one or more times for the file content
        loop {
            stream.read_exact(&mut buf[0..4]).await?;

            if &buf[0..4] == SyncCommand::Data.code() {
                let len = read_length_little_endian(&mut stream).await?;
                stream.read_exact(&mut buf[0..len]).await?;
                buffer.write_all(&buf[0..len]).await?;
            } else if &buf[0..4] == SyncCommand::Done.code() {
                // "DONE" command indicates end of file transfer
                break;
            } else if &buf[0..4] == SyncCommand::Fail.code() {
                let n = buf.len().min(read_length_little_endian(&mut stream).await?);

                stream.read_exact(&mut buf[0..n]).await?;

                let message = std::str::from_utf8(&buf[0..n])
                    .map(|s| format!("adb error: {}", s))
                    .unwrap_or_else(|_| "adb error was not utf-8".into());

                return Err(DeviceError::Adb(message));
            } else {
                return Err(DeviceError::Adb("FAIL (unknown)".to_owned()));
            }
        }

        Ok(())
    }

    pub async fn pull_dir(&self, src: &UnixPath, dest_dir: &Path) -> Result<()> {
        let src = src.to_path_buf();
        let dest_dir = dest_dir.to_path_buf();

        for entry in self.list_dir(&src).await? {
            match entry.metadata {
                RemoteMetadata::RemoteSymlink => {} // Ignored.
                RemoteMetadata::RemoteDir => {
                    let mut d = dest_dir.clone();
                    d.push(&entry.name);

                    std::fs::create_dir_all(&d)?;
                }
                RemoteMetadata::RemoteFile(_) => {
                    let mut s = src.clone();
                    s.push(&entry.name);
                    let mut d = dest_dir.clone();
                    d.push(&entry.name);

                    self.pull(&s, &mut File::create(d).await?).await?;
                }
            }
        }

        Ok(())
    }

    pub async fn push<R: AsyncRead + Unpin>(
        &self,
        buffer: &mut R,
        dest: &UnixPath,
        mode: u32,
    ) -> Result<()> {
        // Implement the ADB protocol to send a file to the device.
        // The protocol consists of the following steps:
        // * Send "host:transport" command with device serial
        // * Send "sync:" command to initialize file transfer
        // * Send "SEND" command with name and mode of the file
        // * Send "DATA" command one or more times for the file content
        // * Send "DONE" command to indicate end of file transfer

        let enable_run_as = self.enable_run_as_for_path(&dest.to_path_buf());
        let dest1 = match enable_run_as {
            true => self.tempfile.as_path(),
            false => UnixPath::new(dest),
        };

        // If the destination directory does not exist, adb will
        // create it and any necessary ancestors however it will not
        // set the directory permissions to 0o777.  In addition,
        // Android 9 (P) has a bug in its push implementation which
        // will cause a push which creates directories to fail with
        // the error `secure_mkdirs failed: Operation not
        // permitted`. We can work around this by creating the
        // destination directories prior to the push.  Collect the
        // ancestors of the destination directory which do not yet
        // exist so we can create them and adjust their permissions
        // prior to performing the push.
        let mut current = dest.parent();
        let mut leaf: Option<&UnixPath> = None;
        let mut root: Option<&UnixPath> = None;

        while let Some(path) = current {
            if self.path_exists(path, enable_run_as).await? {
                break;
            }
            if leaf.is_none() {
                leaf = Some(path);
            }
            root = Some(path);
            current = path.parent();
        }

        if let Some(path) = leaf {
            self.create_dir(path).await?;
        }

        if let Some(path) = root {
            self.chmod(path, "777", true).await?;
        }

        let mut stream = self.host.connect().await?;

        let message = encode_message(&format!("host:transport:{}", self.serial))?;
        stream.write_all(message.as_bytes()).await?;
        let _bytes = read_response(&mut stream, false, true).await?;

        let message = encode_message("sync:")?;
        stream.write_all(message.as_bytes()).await?;
        let _bytes = read_response(&mut stream, false, true).await?;

        stream.write_all(SyncCommand::Send.code()).await?;
        let args_ = format!("{},{}", dest1.display(), mode);
        let args = args_.as_bytes();
        write_length_little_endian(&mut stream, args.len()).await?;
        stream.write_all(args).await?;

        // Use a 32KB buffer to transfer the file contents
        // TODO: Maybe adjust to maxdata (256KB)
        let mut buf = [0; 32 * 1024];

        loop {
            let len = buffer.read(&mut buf).await?;

            if len == 0 {
                break;
            }

            stream.write_all(SyncCommand::Data.code()).await?;
            write_length_little_endian(&mut stream, len).await?;
            stream.write_all(&buf[0..len]).await?;
        }

        // https://android.googlesource.com/platform/system/core/+/master/adb/SYNC.TXT#66
        //
        // When the file is transferred a sync request "DONE" is sent, where length is set
        // to the last modified time for the file. The server responds to this last
        // request (but not to chunk requests) with an "OKAY" sync response (length can
        // be ignored).
        let time: u32 = ((SystemTime::now().duration_since(SystemTime::UNIX_EPOCH))
            .unwrap()
            .as_secs()
            & 0xFFFF_FFFF) as u32;

        stream.write_all(SyncCommand::Done.code()).await?;
        write_length_little_endian(&mut stream, time as usize).await?;

        // Status.
        stream.read_exact(&mut buf[0..4]).await?;

        if buf.starts_with(SyncCommand::Okay.code()) {
            if enable_run_as {
                // Use cp -a to preserve the permissions set by push.
                let result = self
                    .execute_host_shell_command_as(
                        format!("cp -aR {} {}", dest1.display(), dest.display()).as_str(),
                        enable_run_as,
                    )
                    .await;
                if self.remove(dest1).await.is_err() {
                    warn!("Failed to remove {}", dest1.display());
                }
                result?;
            }
            Ok(())
        } else if buf.starts_with(SyncCommand::Fail.code()) {
            if enable_run_as && self.remove(dest1).await.is_err() {
                warn!("Failed to remove {}", dest1.display());
            }
            let n = buf.len().min(read_length_little_endian(&mut stream).await?);

            stream.read_exact(&mut buf[0..n]).await?;

            let message = std::str::from_utf8(&buf[0..n])
                .map(|s| format!("adb error: {}", s))
                .unwrap_or_else(|_| "adb error was not utf-8".into());

            Err(DeviceError::Adb(message))
        } else {
            if self.remove(dest1).await.is_err() {
                warn!("Failed to remove {}", dest1.display());
            }
            Err(DeviceError::Adb("FAIL (unknown)".to_owned()))
        }
    }

    pub async fn push_dir(&self, source: &Path, dest_dir: &UnixPath, mode: u32) -> Result<()> {
        debug!("Pushing {} to {}", source.display(), dest_dir.display());

        let walker = WalkDir::new(source).follow_links(false).into_iter();

        for entry in walker {
            let entry = entry?;
            let path = entry.path();

            if !entry.metadata()?.is_file() {
                continue;
            }

            let mut file = File::open(path).await?;

            let tail = path
                .strip_prefix(source)
                .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;

            let dest = append_components(dest_dir, tail)?;
            self.push(&mut file, &dest, mode).await?;
        }

        Ok(())
    }

    pub async fn remove(&self, path: &UnixPath) -> Result<()> {
        debug!("Deleting {}", path.display());

        self.execute_host_shell_command_as(
            &format!("rm -rf {}", path.display()),
            self.enable_run_as_for_path(path),
        )
        .await?;

        Ok(())
    }
}

pub(crate) fn append_components(
    base: &UnixPath,
    tail: &Path,
) -> std::result::Result<UnixPathBuf, io::Error> {
    let mut buf = base.to_path_buf();

    for component in tail.components() {
        if let Component::Normal(segment) = component {
            let utf8 = segment.to_str().ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::Other,
                    "Could not represent path segment as UTF-8",
                )
            })?;
            buf.push(utf8);
        } else {
            return Err(io::Error::new(
                io::ErrorKind::Other,
                "Unexpected path component".to_owned(),
            ));
        }
    }

    Ok(buf)
}
