//! Read-only saved-connection loader for the minimal remote Git helper client.

use std::path::PathBuf;

use mt_config::SshConnection;
use mt_git_helper::{DiffSide, Operation};
use mt_remote::RemoteGitClient;
use serde::Deserialize;

const USAGE: &str = "Usage: mt-remote-git [--config <projection.json>] --connection <saved-name-or-id> --helper <remote-absolute-path> --root <remote-workspace> <command>
Commands:
  discover
  status <repo>
  diff <repo> <path> [staged|unstaged|working]
  stage <repo> <paths...>
  unstage <repo> <paths...>
  stage-all <repo>
  unstage-all <repo>
  commit <repo> <message>
All repo arguments are remote absolute repository roots. Quote each path/message as one local argument.
The default config is the installed mini-term config.json projection (same locator as SSH sidecars).
--config reads that projection and credential.key from the same directory; it never creates/migrates config.
A failed write is never retried: verify remote repository state before trying another write.";

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SshProjection {
    ssh_connections: Vec<SshConnection>,
}

struct Arguments {
    config: Option<PathBuf>,
    connection: String,
    helper: String,
    root: String,
    operation: Operation,
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.as_slice() == ["--help"] || args.as_slice() == ["-h"] {
        println!("{USAGE}");
        return;
    }
    if let Err(error) = parse_arguments(&args).and_then(run) {
        eprintln!("mt-remote-git: {error}");
        std::process::exit(1);
    }
}

fn parse_arguments(args: &[String]) -> Result<Arguments, String> {
    let mut config = None;
    let mut connection = None;
    let mut helper = None;
    let mut root = None;
    let mut index = 0;
    while index < args.len() && args[index].starts_with("--") {
        let option = args[index].as_str();
        let value = args
            .get(index + 1)
            .ok_or_else(|| format!("missing value for {option}\n{USAGE}"))?;
        match option {
            "--config" if config.is_none() => config = Some(PathBuf::from(value)),
            "--connection" if connection.is_none() => connection = Some(value.clone()),
            "--helper" if helper.is_none() => helper = Some(value.clone()),
            "--root" if root.is_none() => root = Some(value.clone()),
            _ => return Err(format!("unknown or duplicate option\n{USAGE}")),
        }
        index += 2;
    }
    let command = &args[index..];
    let operation = match command.first().map(String::as_str) {
        Some("discover") if command.len() == 1 => Operation::Discover,
        Some("status") if command.len() == 2 => Operation::Status {
            repo: command[1].clone(),
        },
        Some("diff") if command.len() == 3 || command.len() == 4 => {
            let side = match command.get(3).map(String::as_str).unwrap_or("working") {
                "staged" => DiffSide::Staged,
                "unstaged" => DiffSide::Unstaged,
                "working" => DiffSide::Working,
                _ => return Err("diff side must be staged, unstaged, or working".into()),
            };
            Operation::Diff {
                repo: command[1].clone(),
                path: command[2].clone(),
                old_path: None,
                side,
            }
        }
        Some("stage") if command.len() >= 3 => Operation::Stage {
            repo: command[1].clone(),
            paths: command[2..].to_vec(),
        },
        Some("unstage") if command.len() >= 3 => Operation::Unstage {
            repo: command[1].clone(),
            paths: command[2..].to_vec(),
        },
        Some("stage-all") if command.len() == 2 => Operation::StageAll {
            repo: command[1].clone(),
        },
        Some("unstage-all") if command.len() == 2 => Operation::UnstageAll {
            repo: command[1].clone(),
        },
        Some("commit") if command.len() == 3 => Operation::Commit {
            repo: command[1].clone(),
            message: command[2].clone(),
        },
        _ => return Err(USAGE.into()),
    };
    Ok(Arguments {
        config,
        connection: connection
            .filter(|v| !v.is_empty())
            .ok_or_else(|| format!("--connection is required\n{USAGE}"))?,
        helper: helper.ok_or_else(|| format!("--helper is required\n{USAGE}"))?,
        root: root.ok_or_else(|| format!("--root is required\n{USAGE}"))?,
        operation,
    })
}

fn run(args: Arguments) -> Result<(), String> {
    let explicit_config = args.config.is_some();
    let config = args
        .config
        .or_else(mt_core::config_json_path)
        .ok_or("cannot locate the installed mini-term config.json projection")?;
    let bytes = std::fs::read(&config)
        .map_err(|e| format!("cannot read SSH projection {}: {e}", config.display()))?;
    let projection: SshProjection = serde_json::from_slice(&bytes).map_err(|e| {
        // Never render serde's value-bearing Display: malformed config may contain passwords.
        format!(
            "invalid SSH projection {} ({:?}, line {}, column {})",
            config.display(),
            e.classify(),
            e.line(),
            e.column()
        )
    })?;
    let mut matches = projection
        .ssh_connections
        .into_iter()
        .filter(|conn| conn.id == args.connection || conn.name == args.connection);
    let connection = matches
        .next()
        .ok_or("saved SSH connection not found (use its exact name or id)")?;
    if matches.next().is_some() {
        return Err(
            "saved SSH connection selector is ambiguous; use a unique connection id".into(),
        );
    }
    if explicit_config {
        let absolute = std::fs::canonicalize(&config)
            .map_err(|e| format!("cannot resolve SSH projection path: {e}"))?;
        let directory = absolute
            .parent()
            .ok_or("SSH projection has no parent directory")?;
        let vault = mt_secret::Vault::open(directory)
            .map_err(|e| format!("cannot read credential key beside SSH projection: {e}"))?;
        if !mt_secret::install(vault) {
            return Err("cannot install the SSH projection's credential key".into());
        }
    }
    let mut client = RemoteGitClient::connect(&connection, &args.helper, &args.root)?;
    let result = (|| {
        let discovery = client.request(Operation::Discover)?;
        if matches!(args.operation, Operation::Discover) {
            return Ok(discovery);
        }
        client.request(args.operation)
    })();
    let cleanup = client.close();
    let reply = match (result, cleanup) {
        (Ok(reply), Ok(())) => reply,
        (Err(error), Ok(())) => return Err(error),
        (Ok(_), Err(error)) => {
            return Err(format!(
                "remote operation returned success, but helper cleanup failed: {error}; do not repeat the operation"
            ));
        }
        (Err(error), Err(cleanup)) => return Err(format!("{error}; {cleanup}")),
    };
    let stdout = std::io::stdout();
    let mut output = stdout.lock();
    serde_json::to_writer_pretty(&mut output, &reply)
        .map_err(|e| format!("writing Git reply: {e}"))?;
    use std::io::Write;
    writeln!(output).map_err(|e| format!("writing Git reply: {e}"))
}
