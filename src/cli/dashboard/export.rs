//! Clipboard copies and plain-text exports without interrupting the member.

use std::{
    fs::OpenOptions,
    io::{self, Write as _},
    path::PathBuf,
};

use crossterm::clipboard::CopyToClipboard;

#[derive(Debug)]
pub(super) enum Export {
    Copy {
        contents: String,
        description: String,
    },
    Save {
        path: PathBuf,
        contents: String,
    },
}

impl Export {
    pub(super) async fn execute(self) -> Result<String, Error> {
        match self {
            Self::Copy {
                contents,
                description,
            } => {
                #[cfg(target_os = "macos")]
                if std::env::var_os("SSH_TTY").is_none()
                    && std::env::var_os("SSH_CONNECTION").is_none()
                {
                    Self::copy_to_process(
                        contents,
                        tokio::process::Command::new("/usr/bin/pbcopy"),
                    )
                    .await?;
                    return Ok(format!("Copied {description}"));
                }
                crossterm::execute!(
                    io::stdout().lock(),
                    CopyToClipboard::to_clipboard_from(contents)
                )
                .map_err(Error::Clipboard)?;
                Ok(format!(
                    "Sent {description} to terminal clipboard · clipboard access must be enabled"
                ))
            }
            Self::Save { path, contents } => {
                let destination = path.clone();
                tokio::task::spawn_blocking(move || {
                    let mut file = OpenOptions::new().write(true).create_new(true).open(path)?;
                    file.write_all(contents.as_bytes())
                })
                .await?
                .map_err(|source| Error::File {
                    path: destination.clone(),
                    source,
                })?;
                Ok(format!("Saved log to {}", destination.display()))
            }
        }
    }

    #[cfg(any(test, target_os = "macos"))]
    pub(super) async fn copy_to_process(
        contents: String,
        mut command: tokio::process::Command,
    ) -> Result<(), Error> {
        use tokio::io::AsyncWriteExt as _;

        let mut child = command
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .map_err(Error::Clipboard)?;
        let mut input = child
            .stdin
            .take()
            .ok_or_else(|| Error::Clipboard(io::Error::other("clipboard input unavailable")))?;
        input
            .write_all(contents.as_bytes())
            .await
            .map_err(Error::Clipboard)?;
        drop(input);
        let status = child.wait().await.map_err(Error::Clipboard)?;
        if !status.success() {
            return Err(Error::Clipboard(io::Error::other(format!(
                "clipboard program exited with {status}"
            ))));
        }
        Ok(())
    }
}

#[derive(Debug)]
pub(super) struct SavePrompt {
    pub(super) path: String,
    pub(super) contents: String,
}

#[derive(Debug, thiserror::Error)]
pub(super) enum Error {
    #[error("could not save log to '{path}': {source}")]
    File { path: PathBuf, source: io::Error },
    #[error("could not copy to clipboard: {0}")]
    Clipboard(io::Error),
    #[error("dashboard export failed: {0}")]
    Worker(#[from] tokio::task::JoinError),
}
