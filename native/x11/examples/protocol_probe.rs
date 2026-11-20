//! Private-display protocol acceptance host; application engines live elsewhere.
use std::{
    io::Write,
    time::{Duration, Instant},
};
use zinput_x11::{Context, Event, InputMethod, PopupFrame};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let name = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "zinput-test".into());
    let mut input = InputMethod::connect_named(&name)?;
    let mut previous = Context::default();
    let pixels = [32u8, 48, 64, 255].repeat(80 * 30);
    println!("READY");
    std::io::stdout().flush()?;
    let deadline = Instant::now() + Duration::from_secs(120);
    while Instant::now() < deadline {
        input.dispatch(Duration::from_millis(50))?;
        while let Some(event) = input.next_event()? {
            match event {
                Event::State(context) => {
                    assert_eq!(context, input.context());
                    if previous.active && previous != context {
                        assert!(input.update(&previous, "", 0, Some("old")).is_err());
                    }
                    previous = context;
                    println!("STATE {} {}", context.generation, context.active);
                }
                Event::Key(key) => {
                    assert_eq!(key.context, input.context());
                    if key.pressed {
                        println!("KEYSYM {:x} {} {}", key.keysym, key.keycode, key.repeat);
                    }
                    if !key.pressed {
                        input.forward_key(&key)?;
                        continue;
                    }
                    if key.modifiers & (2 | 4 | 8) != 0 {
                        input.forward_key(&key)?;
                    } else {
                        match key.keysym {
                            0x74 => input.update(&key.context, "", 0, Some("你好世界"))?,
                            0x72 => input.update(&key.context, "", 0, Some("かなカナ"))?,
                            0x63 => {
                                input.update(&key.context, "ni", 2, None)?;
                                let status = input.present_popup(
                                    &key.context,
                                    PopupFrame {
                                        width: 80,
                                        height: 30,
                                        scale: 1,
                                        pixels: &pixels,
                                    },
                                )?;
                                println!("POPUP {status:?}");
                            }
                            0x76 => {
                                assert!(input.update(&key.context, "你好", 1, None).is_err());
                                assert!(
                                    input
                                        .update(&key.context, "", 0, Some(&"a".repeat(4001)))
                                        .is_err()
                                );
                                println!("REJECTED");
                            }
                            0x64 => {
                                input.forward_key(&key)?;
                                assert!(input.forward_key(&key).is_err());
                                println!("DUPLICATE_REJECTED");
                            }
                            0x71 => return Ok(()),
                            _ => input.forward_key(&key)?,
                        }
                    }
                }
            }
            std::io::stdout().flush()?;
        }
    }
    Err("protocol probe deadline expired".into())
}
