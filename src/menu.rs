use crate::UserEvent;
use muda::accelerator::{Accelerator, Code, Modifiers};
use muda::{Menu, MenuEvent, MenuItem, PredefinedMenuItem, Submenu};
use winit::event_loop::EventLoopProxy;

pub const PREFERENCES_ID: &str = "preferences";
pub const RELOAD_CONFIG_ID: &str = "reload_config";

/// Build and attach the macOS menu bar: an app menu with About, Preferences
/// (Cmd+,), and Quit. Must be called once at startup, and pairs with
/// `EventLoopBuilder::with_default_menu(false)` on the event loop so
/// winit's own placeholder menu doesn't fight this one.
///
/// Returns the `Menu` -- the caller MUST keep it alive for as long as the
/// app runs. `init_for_nsapp` hands the native NSMenu to AppKit, but the
/// native menu items still hold raw pointers back into muda's Rust-side
/// state; dropping this value lets that state (and those pointers) go
/// dangling, which crashes -- often with a bizarre, unrelated-looking
/// panic -- the next time a menu item is clicked. See
/// https://github.com/tauri-apps/muda/issues/233.
#[must_use = "dropping the returned Menu detaches the native menu bar and leaves dangling pointers behind it"]
pub fn install(proxy: EventLoopProxy<UserEvent>) -> Menu {
    let menu = Menu::new();
    let app_menu = Submenu::new("Terminal", true);

    let preferences = MenuItem::with_id(
        PREFERENCES_ID,
        "Preferences...",
        true,
        Some(Accelerator::new(Some(Modifiers::SUPER), Code::Comma)),
    );
    let reload_config = MenuItem::with_id(
        RELOAD_CONFIG_ID,
        "Reload Config",
        true,
        Some(Accelerator::new(Some(Modifiers::SUPER), Code::KeyR)),
    );

    app_menu
        .append_items(&[
            &PredefinedMenuItem::about(None, None),
            &PredefinedMenuItem::separator(),
            &preferences,
            &reload_config,
            &PredefinedMenuItem::separator(),
            &PredefinedMenuItem::quit(None),
        ])
        .expect("failed to build app menu");

    menu.append(&app_menu).expect("failed to attach app menu");
    menu.init_for_nsapp();

    MenuEvent::set_event_handler(Some(move |event: MenuEvent| {
        if event.id() == PREFERENCES_ID {
            let _ = proxy.send_event(UserEvent::OpenSettings);
        } else if event.id() == RELOAD_CONFIG_ID {
            let _ = proxy.send_event(UserEvent::ReloadConfig);
        }
    }));

    menu
}
