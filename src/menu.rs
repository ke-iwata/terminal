use crate::UserEvent;
use muda::accelerator::{Accelerator, Code, Modifiers};
use muda::{Menu, MenuEvent, MenuItem, PredefinedMenuItem, Submenu};
use winit::event_loop::EventLoopProxy;

pub const PREFERENCES_ID: &str = "preferences";
pub const RELOAD_CONFIG_ID: &str = "reload_config";
pub const NEW_TAB_ID: &str = "new_tab";
pub const CLOSE_PANE_ID: &str = "close_pane";
pub const NEXT_TAB_ID: &str = "next_tab";
pub const PREV_TAB_ID: &str = "prev_tab";
pub const SPLIT_RIGHT_ID: &str = "split_right";
pub const SPLIT_DOWN_ID: &str = "split_down";
pub const NEXT_PANE_ID: &str = "next_pane";
pub const PREV_PANE_ID: &str = "prev_pane";

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
    // macOS ignores this label for the application menu (the one right of
    // the Apple logo) and always shows the process/bundle name instead --
    // kept in sync anyway so the source doesn't lie about what's shown.
    let app_menu = Submenu::new("keterm", true);

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

    let shell_menu = Submenu::new("Shell", true);
    let new_tab = MenuItem::with_id(
        NEW_TAB_ID,
        "New Tab",
        true,
        Some(Accelerator::new(Some(Modifiers::SUPER), Code::KeyT)),
    );
    // "Close", not "Close Tab": Cmd+W closes the focused pane first and
    // only closes the tab when it's the last pane -- same wording and
    // behavior as iTerm2.
    let close_pane = MenuItem::with_id(
        CLOSE_PANE_ID,
        "Close",
        true,
        Some(Accelerator::new(Some(Modifiers::SUPER), Code::KeyW)),
    );
    let next_tab = MenuItem::with_id(
        NEXT_TAB_ID,
        "Next Tab",
        true,
        Some(Accelerator::new(Some(Modifiers::SUPER | Modifiers::SHIFT), Code::BracketRight)),
    );
    let prev_tab = MenuItem::with_id(
        PREV_TAB_ID,
        "Previous Tab",
        true,
        Some(Accelerator::new(Some(Modifiers::SUPER | Modifiers::SHIFT), Code::BracketLeft)),
    );
    let split_right = MenuItem::with_id(
        SPLIT_RIGHT_ID,
        "Split Pane Right",
        true,
        Some(Accelerator::new(Some(Modifiers::SUPER), Code::KeyD)),
    );
    let split_down = MenuItem::with_id(
        SPLIT_DOWN_ID,
        "Split Pane Down",
        true,
        Some(Accelerator::new(Some(Modifiers::SUPER | Modifiers::SHIFT), Code::KeyD)),
    );
    let next_pane = MenuItem::with_id(
        NEXT_PANE_ID,
        "Next Pane",
        true,
        Some(Accelerator::new(Some(Modifiers::SUPER), Code::BracketRight)),
    );
    let prev_pane = MenuItem::with_id(
        PREV_PANE_ID,
        "Previous Pane",
        true,
        Some(Accelerator::new(Some(Modifiers::SUPER), Code::BracketLeft)),
    );
    shell_menu
        .append_items(&[
            &new_tab,
            &close_pane,
            &PredefinedMenuItem::separator(),
            &split_right,
            &split_down,
            &next_pane,
            &prev_pane,
            &PredefinedMenuItem::separator(),
            &next_tab,
            &prev_tab,
        ])
        .expect("failed to build shell menu");

    menu.append(&app_menu).expect("failed to attach app menu");
    menu.append(&shell_menu).expect("failed to attach shell menu");
    menu.init_for_nsapp();

    MenuEvent::set_event_handler(Some(move |event: MenuEvent| {
        let user_event = if event.id() == PREFERENCES_ID {
            UserEvent::OpenSettings
        } else if event.id() == RELOAD_CONFIG_ID {
            UserEvent::ReloadConfig
        } else if event.id() == NEW_TAB_ID {
            UserEvent::NewTab
        } else if event.id() == CLOSE_PANE_ID {
            UserEvent::ClosePane
        } else if event.id() == NEXT_TAB_ID {
            UserEvent::NextTab
        } else if event.id() == PREV_TAB_ID {
            UserEvent::PrevTab
        } else if event.id() == SPLIT_RIGHT_ID {
            UserEvent::SplitRight
        } else if event.id() == SPLIT_DOWN_ID {
            UserEvent::SplitDown
        } else if event.id() == NEXT_PANE_ID {
            UserEvent::NextPane
        } else if event.id() == PREV_PANE_ID {
            UserEvent::PrevPane
        } else {
            return;
        };
        let _ = proxy.send_event(user_event);
    }));

    menu
}
