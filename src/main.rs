use serialport::{SerialPortInfo, SerialPortType, UsbPortInfo};
use std::process::ExitCode;

fn main() -> ExitCode {
    // Tiny hand-rolled flag parse (no clap dep yet): --all / -a shows every
    // port, including the phantom legacy slots we normally hide.
    let show_all = std::env::args().skip(1).any(|a| a == "--all" || a == "-a");

    // `available_ports()` does the OS-specific discovery and hands back a Vec.
    // It can fail (e.g. permissions / platform quirks), so it returns a Result.
    let ports = match serialport::available_ports() {
        Ok(ports) => ports,
        Err(e) => {
            eprintln!("error: failed to enumerate serial ports: {e}");
            return ExitCode::FAILURE;
        }
    };

    // Hide phantom `/dev/ttyS*` slots unless the user asked for everything.
    // `.collect()` into a Vec of borrows — we don't need to own the infos.
    let visible: Vec<&SerialPortInfo> = ports
        .iter()
        .filter(|p| show_all || !is_phantom(p))
        .collect();

    if visible.is_empty() {
        if ports.is_empty() {
            println!("No serial ports found.");
        } else {
            // Everything we found was a phantom slot (else `visible` wouldn't be
            // empty), so the total count is the hidden count.
            println!(
                "No usable serial ports found ({} phantom slot(s) hidden).\n\
                 Plug in a device, or pass --all to see everything.",
                ports.len()
            );
        }
        return ExitCode::SUCCESS;
    }

    println!("Found {} serial port(s):\n", visible.len());

    // `enumerate()` pairs each item with its index; we +1 for a human-friendly
    // 1-based list (this becomes the pick number in Slice 2).
    for (i, port) in visible.iter().enumerate() {
        println!("  [{}] {}", i + 1, port.port_name);
        println!("      {}", describe(&port.port_type));
    }

    ExitCode::SUCCESS
}

/// Is this a phantom legacy UART slot the kernel always exposes even with no
/// chip behind it? Linux publishes each `/dev/ttySN` slot's UART model in
/// `/sys/class/tty/ttySN/type`; `0` == `PORT_UNKNOWN` == "no hardware" → hide.
///
/// Pure file read — no device `open()`, no ioctl, no `unsafe`. Anything we
/// can't positively identify as phantom is kept (fail open, never hide a real
/// port).
fn is_phantom(port: &SerialPortInfo) -> bool {
    // USB / Bluetooth / PCI presence already proves the port is real; only the
    // metadata-less `Unknown` legacy ports are candidates.
    if !matches!(port.port_type, SerialPortType::Unknown) {
        return false;
    }

    // `let ... else` binds on success or bails on the `None`/non-match path.
    let Some(name) = port.port_name.strip_prefix("/dev/") else {
        return false; // not a Linux /dev path (e.g. Windows COM*)
    };
    if !name.starts_with("ttyS") {
        return false; // only the legacy 8250/16550 family can be phantom
    }

    match std::fs::read_to_string(format!("/sys/class/tty/{name}/type")) {
        Ok(contents) => contents.trim() == "0",
        Err(_) => false, // can't read sysfs → can't prove phantom → keep it
    }
}

/// Turn the port's type into a one-line, human-readable description.
fn describe(port_type: &SerialPortType) -> String {
    match port_type {
        SerialPortType::UsbPort(info) => describe_usb(info),
        SerialPortType::BluetoothPort => "Bluetooth".to_string(),
        SerialPortType::PciPort => "PCI".to_string(),
        SerialPortType::Unknown => "Unknown".to_string(),
    }
}

/// USB ports carry extra metadata (vendor/product strings, VID:PID) that make
/// the list much easier to recognize — surface whatever the OS gave us.
fn describe_usb(info: &UsbPortInfo) -> String {
    // `as_deref()` turns `&Option<String>` into `Option<&str>` so we can fall
    // back to a placeholder without cloning.
    let manufacturer = info.manufacturer.as_deref().unwrap_or("unknown vendor");
    let product = info.product.as_deref().unwrap_or("unknown product");

    format!(
        "USB  {manufacturer} — {product}  (VID:PID {:04x}:{:04x})",
        info.vid, info.pid
    )
}
