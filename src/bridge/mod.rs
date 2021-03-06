mod events;
mod handler;
mod keybindings;
mod ui_commands;

use std::sync::Arc;
use std::process::Stdio;

use rmpv::Value;
use nvim_rs::{create::tokio as create, UiAttachOptions, Neovim};
use nvim_rs::compat::tokio::Compat;
use tokio::runtime::Runtime;
use tokio::process::{Command, ChildStdin};
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};

pub use events::*;
pub use keybindings::*;
pub use ui_commands::UiCommand;
use crate::error_handling::ResultPanicExplanation;
use crate::INITIAL_DIMENSIONS;
use handler::NeovimHandler;

lazy_static! {
    pub static ref BRIDGE: Bridge = Bridge::new();
}

#[cfg(target_os = "windows")]
fn set_windows_creation_flags(cmd: &mut Command) {
    cmd.creation_flags(0x08000000); // CREATE_NO_WINDOW
}

fn create_nvim_command() -> Command {
    let mut cmd = Command::new("nvim");

    cmd.arg("--embed")
        .args(std::env::args().skip(1))
        .stderr(Stdio::inherit());

    #[cfg(target_os = "windows")]
    set_windows_creation_flags(&mut cmd);

    cmd
}

async fn drain(receiver: &mut UnboundedReceiver<UiCommand>) -> Option<Vec<UiCommand>> {
    if let Some(ui_command) = receiver.recv().await {
        let mut results = vec![ui_command];
        while let Ok(ui_command) = receiver.try_recv() {
            results.push(ui_command);
        }
        Some(results)
    } else {
        None
    }
}

async fn handle_current_commands(receiver: &mut UnboundedReceiver<UiCommand>, nvim: &Neovim<Compat<ChildStdin>>) -> bool {
    if let Some(commands) = drain(receiver).await {
        let (resize_list, other_commands): (Vec<UiCommand>, Vec<UiCommand>) = commands
            .into_iter()
            .partition(|command| command.is_resize());
        if let Some(resize_command) = resize_list.into_iter().last() {
            resize_command.execute(&nvim).await;
        }

        for ui_command in other_commands.into_iter() {
            ui_command.execute(&nvim).await;
        }
        true
    } else {
        false
    }
}

async fn start_process(mut receiver: UnboundedReceiver<UiCommand>) {
    let (width, height) = INITIAL_DIMENSIONS;
    let (mut nvim, io_handler, _) = create::new_child_cmd(&mut create_nvim_command(), NeovimHandler::new()).await
        .unwrap_or_explained_panic("Could not create nvim process", "Could not locate or start the neovim process");

    tokio::spawn(async move {
        match io_handler.await {
            Err(join_error) => eprintln!("Error joining IO loop: '{}'", join_error),
            Ok(Err(error)) => eprintln!("Error: '{}'", error),
            Ok(Ok(())) => {}
        };
        std::process::exit(0);
    });

    nvim.set_var("neovide", Value::Boolean(true)).await
        .unwrap_or_explained_panic("Could not communicate.", "Could not communicate with neovim process");
    let mut options = UiAttachOptions::new();
    options.set_linegrid_external(true);
    options.set_rgb(true);
    nvim.ui_attach(width as i64, height as i64, &options).await
        .unwrap_or_explained_panic("Could not attach.", "Could not attach ui to neovim process");

    let nvim = Arc::new(nvim);
    tokio::spawn(async move {
        loop {
            if !handle_current_commands(&mut receiver, &nvim).await {
                break;
            }
        }
    });
}

pub struct Bridge {
    _runtime: Runtime,
    sender: UnboundedSender<UiCommand>
}

impl Bridge {
    pub fn new() -> Bridge {
        let mut runtime = Runtime::new().unwrap();
        let (sender, receiver) = unbounded_channel::<UiCommand>();

        runtime.block_on(async move {
            start_process(receiver).await;
        });

        Bridge { _runtime: runtime, sender }
    }

    pub fn queue_command(&self, command: UiCommand) {
        self.sender.send(command)
            .unwrap_or_explained_panic(
                "Could Not Send UI Command", 
                "Could not send UI command from the window system to the neovim process.");
    }
}
