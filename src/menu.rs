use crate::UserEvent;
use muda::accelerator::{Accelerator, Code, Modifiers};
use muda::{Menu, MenuEvent, MenuItem, PredefinedMenuItem, Submenu};
use winit::event_loop::EventLoopProxy;

pub const PREFERENCES_ID: &str = "preferences";

/// Build and attach the macOS menu bar: an app menu with About, Preferences
/// (Cmd+,), and Quit. Must be called once at startup, and pairs with
/// `EventLoopBuilder::with_default_menu(false)` on the event loop so
/// winit's own placeholder menu doesn't fight this one.
pub fn install(proxy: EventLoopProxy<UserEvent>) {
    let menu = Menu::new();
    let app_menu = Submenu::new("Terminal", true);

    let preferences = MenuItem::with_id(
        PREFERENCES_ID,
        "Preferences...",
        true,
        Some(Accelerator::new(Some(Modifiers::SUPER), Code::Comma)),
    );

    app_menu
        .append_items(&[
            &PredefinedMenuItem::about(None, None),
            &PredefinedMenuItem::separator(),
            &preferences,
            &PredefinedMenuItem::separator(),
            &PredefinedMenuItem::quit(None),
        ])
        .expect("failed to build app menu");

    menu.append(&app_menu).expect("failed to attach app menu");
    menu.init_for_nsapp();

    MenuEvent::set_event_handler(Some(move |event: MenuEvent| {
        if event.id() == PREFERENCES_ID {
            let _ = proxy.send_event(UserEvent::OpenSettings);
        }
    }));
}
