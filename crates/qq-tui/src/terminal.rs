use std::io::{self, stdout};

use crossterm::{
    cursor::{Hide, MoveTo, Show},
    event::{
        DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
        EventStream,
    },
    execute,
    style::{Attribute, Print, ResetColor, SetAttribute},
    terminal::{self, Clear, ClearType, EndSynchronizedUpdate},
};
use futures_util::StreamExt;
use tokio::{
    io::AsyncWriteExt,
    time::{Duration, MissedTickBehavior, interval},
};

use crate::{
    ClientPort, ClientRequest, ClientUpdate,
    app::{App, TuiError},
    view::FrameRenderer,
};

pub async fn run<P>(mut client: P, mut app: App) -> Result<(), TuiError>
where
    P: ClientPort,
{
    let shutdown = shutdown_signal();
    tokio::pin!(shutdown);
    tokio::select! {
        biased;
        result = &mut shutdown => return result.map_err(TuiError::from),
        _ = tokio::task::yield_now() => {}
    }

    let _terminal = TerminalGuard::enter()?;
    let mut terminal_events = EventStream::new();
    let mut output = tokio::io::stdout();
    let mut renderer = FrameRenderer::default();
    let mut frame_tick = interval(Duration::from_millis(8));
    let mut animation_tick = interval(Duration::from_millis(125));
    frame_tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
    animation_tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut dirty = true;

    loop {
        tokio::select! {
            biased;
            result = &mut shutdown => {
                result?;
                break;
            }
            event = terminal_events.next() => {
                match event {
                    Some(Ok(event)) => {
                        let (changed, requests) = app.handle_terminal_event(event);
                        dirty |= changed;
                        for request in requests {
                            if let Err(error) = client.try_send(request.clone()) {
                                dirty |= apply_send_failure(&mut app, request, error);
                            }
                        }
                    }
                    Some(Err(error)) => return Err(TuiError::Terminal(error)),
                    None => break,
                }
            }
            update = client.recv() => {
                let Some(update) = update else {
                    return Err(TuiError::ClientStopped);
                };
                dirty |= app.apply_client_update(update);
            }
            _ = animation_tick.tick(), if app.has_activity() => {
                dirty |= app.advance_animation();
            }
            _ = frame_tick.tick(), if dirty => {
                let bytes = renderer.draw(&mut app)?;
                output.write_all(&bytes).await?;
                output.flush().await?;
                dirty = false;
            }
        }
        if app.quit {
            break;
        }
    }
    Ok(())
}

fn apply_send_failure(app: &mut App, request: ClientRequest, error: crate::ClientFailure) -> bool {
    let update = match request {
        ClientRequest::Command(command) => ClientUpdate::CommandResult {
            command_id: command.command_id,
            result: Err(error),
        },
        ClientRequest::Snapshot(_) => ClientUpdate::SnapshotFailed(error),
    };
    app.apply_client_update(update)
}

struct TerminalGuard;

impl TerminalGuard {
    fn enter() -> io::Result<Self> {
        terminal::enable_raw_mode()?;
        let guard = Self;
        let mut output = stdout();
        enable_input_modes(&mut output)?;
        execute!(output, Hide, Clear(ClearType::All), MoveTo(0, 0))?;
        Ok(guard)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = terminal::disable_raw_mode();
        let (_, height) = terminal::size().unwrap_or((80, 24));
        let mut output = stdout();
        let _ = execute!(
            output,
            SetAttribute(Attribute::Reset),
            ResetColor,
            EndSynchronizedUpdate
        );
        let _ = disable_input_modes(&mut output);
        let _ = execute!(
            output,
            MoveTo(0, height.saturating_sub(1)),
            Clear(ClearType::CurrentLine),
            Show,
            Print("\r\n")
        );
    }
}

fn enable_input_modes(output: &mut impl io::Write) -> io::Result<()> {
    execute!(output, EnableBracketedPaste, EnableMouseCapture)
}

fn disable_input_modes(output: &mut impl io::Write) -> io::Result<()> {
    execute!(output, DisableMouseCapture, DisableBracketedPaste)
}

#[cfg(unix)]
async fn shutdown_signal() -> io::Result<()> {
    use tokio::signal::unix::{SignalKind, signal};

    let mut terminate = signal(SignalKind::terminate())?;
    let mut hangup = signal(SignalKind::hangup())?;
    tokio::select! {
        result = tokio::signal::ctrl_c() => result,
        _ = terminate.recv() => Ok(()),
        _ = hangup.recv() => Ok(()),
    }
}

#[cfg(not(unix))]
async fn shutdown_signal() -> io::Result<()> {
    tokio::signal::ctrl_c().await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_input_modes_enable_and_restore_mouse_reporting() {
        let mut entered = Vec::new();
        let mut restored = Vec::new();

        enable_input_modes(&mut entered).unwrap();
        disable_input_modes(&mut restored).unwrap();

        let entered = String::from_utf8(entered).unwrap();
        let restored = String::from_utf8(restored).unwrap();
        assert!(entered.contains("\x1b[?1000h"));
        assert!(entered.contains("\x1b[?2004h"));
        assert!(restored.contains("\x1b[?1000l"));
        assert!(restored.contains("\x1b[?2004l"));
    }
}
