//! Diagnostic: create a virtual display, pump the main run loop, report what the window server sees.
fn main() {
    #[cfg(target_os = "macos")]
    {
        use aether_display::virtual_display::*;
        if std::env::var("WITH_APP").is_ok() {
            let mtm = objc2::MainThreadMarker::new().unwrap();
            let app = objc2_app_kit::NSApplication::sharedApplication(mtm);
            app.setActivationPolicy(objc2_app_kit::NSApplicationActivationPolicy::Accessory);
            app.finishLaunching();
            println!("NSApplication initialised");
        }
        println!("before: {} online", online_display_count());
        match VirtualDisplay::create("AetherLink Probe", if std::env::var("LODPI").is_ok() { VirtualDisplaySpec { hidpi: false, ..VirtualDisplaySpec::FHD_16_9 } } else if let Ok(p) = std::env::var("PPI") { VirtualDisplaySpec { ppi: p.parse().unwrap(), ..VirtualDisplaySpec::FHD_16_9 } } else { VirtualDisplaySpec::FHD_16_9 }) {
            Ok(vd) => {
                println!("created id={}", vd.display_id());
                for i in 0..10 {
                    aether_display::ghost::pump_main_runloop(0.2);
                    println!("t+{}ms: {} online", (i + 1) * 200, online_display_count());
                }
                drop(vd);
                aether_display::ghost::pump_main_runloop(0.5);
                println!("after drop: {} online", online_display_count());
            }
            Err(e) => println!("error: {e}"),
        }
    }
}
