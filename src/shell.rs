//! Interactive read-only shell over an image's filesystems, for debugging:
//!
//! ```text
//! lbx p2:/> ls boot
//! lbx p2:/> cd boot/grub
//! lbx p2:/boot/grub> cat grub.cfg
//! lbx p2:/boot/grub> cp p1:/STARTUP.CFG /tmp/
//! ```

use crate::{entry_label, human_size, parse_uri, print_entry, print_listing, write_dest};
use anyhow::{bail, Result};
use lbx::fsys::FileSystem;
use lbx::part::Partition;
use std::io::{self, BufRead, Write};
use std::path::Path;

const HELP: &str = "\
commands:
  parts                 list partitions ('*' = current)
  use <N|pN>            switch to partition N
  pwd / cd [path]       show / change directory
  ls [path]             list directory
  cat <path>            write file to stdout (raw bytes)
  cp <path> <local>     copy file out of the image
  boot                  show boot entries on the current partition
  help                  this text
  exit                  quit (also Ctrl-D)
paths may be relative, absolute, or pN:/... URIs (any partition)";

pub(crate) fn run(fss: Vec<(Partition, Box<dyn FileSystem>)>) -> Result<()> {
    let mut current = 0usize;
    let mut cwd = String::from("/");

    println!("lbx interactive shell — 'help' for commands, 'exit' to quit");
    list_parts(&fss, current);

    let stdin = io::stdin();
    let mut input = stdin.lock();
    loop {
        print!("lbx p{}:{}> ", fss[current].0.index, cwd);
        io::stdout().flush()?;

        let mut line = String::new();
        if input.read_line(&mut line)? == 0 {
            println!();
            break; // EOF
        }
        let words: Vec<&str> = line.split_whitespace().collect();
        let Some(&cmd) = words.first() else { continue };

        let result = match (cmd, &words[1..]) {
            ("help" | "?", _) => {
                println!("{HELP}");
                Ok(())
            }
            ("exit" | "quit" | "q", _) => break,
            ("parts", _) => {
                list_parts(&fss, current);
                Ok(())
            }
            ("use", [which]) => switch_part(&fss, which).map(|i| {
                current = i;
                cwd = "/".to_string();
            }),
            ("pwd", _) => {
                println!("p{}:{}", fss[current].0.index, cwd);
                Ok(())
            }
            ("cd", rest) => {
                let target = resolve(&cwd, rest.first().unwrap_or(&"/"));
                match fss[current].1.read_dir(&target) {
                    Ok(_) => {
                        cwd = target;
                        Ok(())
                    }
                    Err(_) => Err(anyhow::anyhow!("{target}: not a directory")),
                }
            }
            ("ls", rest) => {
                let (fs, path) = locate(&fss, current, &cwd, rest.first().unwrap_or(&"."))?;
                fs.read_dir(&path).map(print_listing).map_err(Into::into)
            }
            ("cat", [path]) => {
                let (fs, path) = locate(&fss, current, &cwd, path)?;
                match fs.read_file(&path) {
                    Ok(data) => {
                        io::stdout().write_all(&data)?;
                        if !data.ends_with(b"\n") {
                            println!();
                        }
                        Ok(())
                    }
                    Err(e) => Err(e.into()),
                }
            }
            ("cp", [src, dest]) => {
                let (fs, path) = locate(&fss, current, &cwd, src)?;
                match fs.read_file(&path) {
                    Ok(data) => write_dest(Path::new(dest), &path, &data).map(|written| {
                        println!("{} ({})", written.display(), human_size(data.len() as u64));
                    }),
                    Err(e) => Err(e.into()),
                }
            }
            ("boot" | "entries", _) => show_boot(&fss[current]),
            ("cat", _) => Err(anyhow::anyhow!("usage: cat <path>")),
            ("cp", _) => Err(anyhow::anyhow!("usage: cp <path> <local dest>")),
            ("use", _) => Err(anyhow::anyhow!("usage: use <N|pN>")),
            _ => Err(anyhow::anyhow!("unknown command '{cmd}' — try 'help'")),
        };
        if let Err(e) = result {
            eprintln!("error: {e}");
        }
    }
    Ok(())
}

fn list_parts(fss: &[(Partition, Box<dyn FileSystem>)], current: usize) {
    for (i, (p, fs)) in fss.iter().enumerate() {
        let mark = if i == current { "*" } else { " " };
        let label = fs.label().map(|l| format!(" label={l}")).unwrap_or_default();
        println!(
            "{mark} p{}  {:>10}  {:<8} {}{label}",
            p.index,
            human_size(p.size_bytes),
            fs.fs_type(),
            p.kind,
        );
    }
}

fn switch_part(fss: &[(Partition, Box<dyn FileSystem>)], which: &str) -> Result<usize> {
    let n: usize = which.trim_start_matches('p').parse()?;
    match fss.iter().position(|(p, _)| p.index == n) {
        Some(i) => Ok(i),
        None => bail!("no readable filesystem on partition {n}"),
    }
}

/// Resolve a user path against the cwd, folding `.` and `..`.
fn resolve(cwd: &str, input: &str) -> String {
    let base = if input.starts_with('/') { "" } else { cwd };
    let mut parts: Vec<&str> = Vec::new();
    for seg in base.split('/').chain(input.split('/')) {
        match seg {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            s => parts.push(s),
        }
    }
    format!("/{}", parts.join("/"))
}

/// Turn a user path (relative, absolute, or pN: URI) into the filesystem
/// to read from plus a normalized absolute path.
fn locate<'a>(
    fss: &'a [(Partition, Box<dyn FileSystem>)],
    current: usize,
    cwd: &str,
    input: &str,
) -> Result<(&'a dyn FileSystem, String)> {
    let (part, path) = parse_uri(input);
    match part {
        Some(n) => match fss.iter().find(|(p, _)| p.index == n) {
            Some((_, fs)) => Ok((fs.as_ref(), resolve("/", &path))),
            None => bail!("no readable filesystem on partition {n}"),
        },
        None => Ok((fss[current].1.as_ref(), resolve(cwd, input))),
    }
}

fn show_boot(part_fs: &(Partition, Box<dyn FileSystem>)) -> Result<()> {
    let (p, fs) = part_fs;
    let scan = lbx::boot::scan(fs.as_ref())?;
    if scan.entries.is_empty() {
        bail!("no boot entries on partition {}", p.index);
    }
    for (i, entry) in scan.entries.iter().enumerate() {
        let mark = if Some(i) == scan.default { "*" } else { " " };
        println!("{mark} [{}] {} ({})", i + 1, entry_label(entry), entry.source);
        print_entry(p.index, entry, None);
    }
    Ok(())
}
