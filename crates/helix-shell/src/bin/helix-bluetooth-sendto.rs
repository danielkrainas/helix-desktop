//! `helix-bluetooth-sendto [--device=ADDRESS] [--name=NAME] [FILE…]`: send
//! files to a Bluetooth device through the shell (helix-desktop-spec
//! `contracts/dbus/org.bluez.obex.md`, "Sending"). The arguments are those
//! of the usual `bluetooth-sendto`, so file managers can call it.

fn parse(args: impl Iterator<Item = String>) -> Result<(String, Vec<String>), String> {
    let mut device = String::new();
    let mut files = Vec::new();
    let mut args = args.peekable();
    while let Some(arg) = args.next() {
        if let Some(value) = arg.strip_prefix("--device=") {
            device = value.to_owned();
        } else if arg == "--device" {
            device = args.next().ok_or("--device needs an address")?;
        } else if arg.starts_with("--name=") {
        } else if arg == "--name" {
            args.next();
        } else if arg == "--help" || arg == "-h" {
            return Err("usage: helix-bluetooth-sendto [--device=ADDRESS] [--name=NAME] [FILE…]".into());
        } else if arg.starts_with("file:") {
            files.push(arg);
        } else {
            let path = std::path::absolute(&arg).map_err(|e| format!("{arg}: {e}"))?;
            files.push(path.to_string_lossy().into_owned());
        }
    }
    Ok((device, files))
}

fn main() {
    let (device, files) = match parse(std::env::args().skip(1)) {
        Ok(parsed) => parsed,
        Err(message) => {
            eprintln!("{message}");
            std::process::exit(2);
        }
    };
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("a tokio runtime");
    let sent = runtime.block_on(async {
        let conn = zbus::Connection::session().await?;
        conn.call_method(Some("org.gnome.Shell"), helix_shell::obex::SEND_PATH, Some("org.helix.BluetoothSendTo"), "SendFiles", &(device, files)).await
    });
    if let Err(err) = sent {
        eprintln!("cannot send files: {err}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::parse;

    #[test]
    fn arguments() {
        let args = |a: &[&str]| parse(a.iter().map(|s| s.to_string()));
        assert_eq!(args(&["--device=AA:BB", "--name=Phone", "/tmp/a"]).unwrap(), ("AA:BB".into(), vec!["/tmp/a".into()]));
        assert_eq!(args(&["--device", "AA:BB", "file:///x"]).unwrap(), ("AA:BB".into(), vec!["file:///x".into()]));
        let (device, files) = args(&["rel.txt"]).unwrap();
        assert!(device.is_empty() && files[0].ends_with("/rel.txt") && files[0].starts_with('/'));
        assert!(args(&["--device"]).is_err());
    }
}
