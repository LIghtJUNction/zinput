#[cfg(target_os = "linux")]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let capabilities = zinput_wayland::probe()?;
    println!(
        "input_method={} virtual_keyboard={} seat={}",
        capabilities.input_method, capabilities.virtual_keyboard, capabilities.seat
    );
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("zinput-wayland requires Linux and a compatible Wayland compositor");
}
