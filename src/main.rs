use serialport::{SerialPortType, UsbPortInfo};
use std::process::ExitCode;

fn main() -> ExitCode {
    // `available_ports()` does the OS-specific discovery and hands back a Vec.
    // It can fail (e.g. permissions / platform quirks), so it returns a Result.
    let ports = match serialport::available_ports() {
        Ok(ports) => ports,
        Err(e) => {
            eprintln!("error: failed to enumerate serial ports: {e}");
            return ExitCode::FAILURE;
        }
    };

    if ports.is_empty() {
        println!("No serial ports found.");
        return ExitCode::SUCCESS;
    }

    println!("Found {} serial port(s):\n", ports.len());

    // `enumerate()` pairs each item with its index; we +1 for a human-friendly
    // 1-based list (this becomes the pick number in Slice 2).
    for (i, port) in ports.iter().enumerate() {
        println!("  [{}] {}", i + 1, port.port_name);
        println!("      {}", describe(&port.port_type));
    }

    ExitCode::SUCCESS
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
