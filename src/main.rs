mod shell;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use lbx::blockdev::ReadAt;
use lbx::boot::{BootEntry, BootScan, Source};
use lbx::disk::DiskImage;
use lbx::fsys::{self, DirEntry, FileKind, FileSystem};
use lbx::part::{self, Partition};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Read kernels, initramfs and boot configs out of VM disk images
/// (qcow2/raw) without mounting anything.
///
/// The image may be a local path or an `http(s)://` URL — a remote image is
/// analysed lazily, downloading only the ranges actually read. Files inside
/// it are addressed as URIs: `p2:/boot/vmlinuz` reads from partition 2; a
/// bare `/boot/vmlinuz` searches every readable partition.
#[derive(Parser)]
#[command(version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
    /// Cache downloaded chunks of remote (URL) images here for reuse across
    /// runs. Layout: <dir>/<md5(url)>/<chunk-index>.
    #[arg(long, global = true, value_name = "DIR")]
    cache_dir: Option<PathBuf>,
    /// Per-connection network timeout in seconds for remote (URL) images.
    /// Each chunk uses a fresh connection, so a chunk whose connect or read
    /// stalls this long fails (default 60).
    #[arg(long, global = true, value_name = "SECS")]
    timeout: Option<u64>,
}

#[derive(Subcommand)]
enum Command {
    /// Show image format and partition table
    Info { image: String },
    /// Show the boot entry the bootloader would pick (kernel/initrd/cmdline)
    BootInfo {
        image: String,
        /// Machine-readable output
        #[arg(long)]
        json: bool,
    },
    /// List all boot entries found in the image
    Entries {
        image: String,
        /// Machine-readable output
        #[arg(long)]
        json: bool,
    },
    /// List directory contents (path may be a `pN:/...` URI)
    Ls {
        image: String,
        #[arg(default_value = "/")]
        path: String,
        /// Partition index (alternative to a `pN:` URI prefix)
        #[arg(short, long)]
        part: Option<usize>,
    },
    /// Write a file from the image to stdout (path may be a `pN:/...` URI)
    Cat {
        image: String,
        path: String,
        #[arg(short, long)]
        part: Option<usize>,
    },
    /// Copy a file out of the image (source may be a `pN:/...` URI)
    Cp {
        image: String,
        source: String,
        /// Destination file or directory
        dest: PathBuf,
        #[arg(short, long)]
        part: Option<usize>,
        /// If the source is a compressed kernel (gzip `Image.gz` or EFI
        /// zboot), write the decompressed raw `Image` a VMM can direct-boot
        /// (see `entries`' `kernel_compression`); a no-op for anything else
        #[arg(long)]
        decompress: bool,
    },
    /// Print the MD5 of a file in the image, `md5sum`-style (the path may
    /// be a `pN:/...` URI). For comparing an in-image kernel/initrd against
    /// an already-extracted copy without mounting the image.
    Md5 {
        image: String,
        path: String,
        #[arg(short, long)]
        part: Option<usize>,
        /// MD5 the decompressed kernel (as `cp --decompress`/`extract
        /// --decompress` would write it) rather than the raw stored bytes,
        /// so the digest matches the extracted file
        #[arg(long)]
        decompress: bool,
    },
    /// Find and extract kernel + initramfs + boot configs
    Extract {
        image: String,
        /// Output directory
        #[arg(short, long, default_value = ".")]
        output: PathBuf,
        /// Boot entry to extract, as numbered by `entries` (default: the
        /// entry the bootloader would pick)
        #[arg(short, long)]
        entry: Option<usize>,
        /// Only report what would be extracted
        #[arg(long)]
        dry_run: bool,
        /// Rewrite root=/resume= device names to PARTUUID= in the
        /// extracted cmdline (for direct-kernel boot under virtio)
        #[arg(long)]
        vdafix: bool,
        /// Decompress a gzip/zboot-wrapped kernel into the raw `Image` a
        /// VMM can direct-boot, instead of copying the stored bytes verbatim
        #[arg(long)]
        decompress: bool,
    },
    /// Interactive shell for poking around the image (ls/cd/cat/cp/...)
    Shell { image: String },
}

fn main() -> Result<()> {
    // Die quietly on SIGPIPE (e.g. `lbx cat ... | head`) like other
    // stream-oriented CLI tools.
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }

    let Cli { command, cache_dir, timeout } = Cli::parse();
    lbx::source::set_cache_dir(cache_dir);
    lbx::source::set_http_timeout(timeout);
    match command {
        Command::Info { image } => cmd_info(&image),
        Command::BootInfo { image, json } => cmd_boot_info(&image, json),
        Command::Entries { image, json } => cmd_entries(&image, json),
        Command::Ls { image, path, part } => cmd_ls(&image, &path, part),
        Command::Cat { image, path, part } => cmd_cat(&image, &path, part),
        Command::Cp { image, source, dest, part, decompress } => {
            cmd_cp(&image, &source, &dest, part, decompress)
        }
        Command::Md5 { image, path, part, decompress } => {
            cmd_md5(&image, &path, part, decompress)
        }
        Command::Extract { image, output, entry, dry_run, vdafix, decompress } => {
            cmd_extract(&image, &output, entry, dry_run, vdafix, decompress)
        }
        Command::Shell { image } => {
            let disk = open_disk(&image)?;
            shell::run(open_filesystems(&disk, None)?)
        }
    }
}

/// Split a `pN:/path` URI into partition index and path.
/// Bare paths come back with `None` (= search all partitions).
pub(crate) fn parse_uri(s: &str) -> (Option<usize>, String) {
    if let Some(rest) = s.strip_prefix('p')
        && let Some((num, path)) = rest.split_once(':')
        && let Ok(n) = num.parse::<usize>()
    {
        let path = if path.starts_with('/') { path.to_string() } else { format!("/{path}") };
        return (Some(n), path);
    }
    (None, s.to_string())
}

pub(crate) fn uri(part: usize, path: &str) -> String {
    format!("p{part}:{path}")
}

fn open_disk(image: &str) -> Result<Arc<DiskImage>> {
    Ok(Arc::new(
        DiskImage::open(image).with_context(|| format!("opening {image}"))?,
    ))
}

/// (partition, filesystem) pairs for every partition we can actually read.
fn open_filesystems(
    disk: &Arc<DiskImage>,
    only_part: Option<usize>,
) -> Result<Vec<(Partition, Box<dyn FileSystem>)>> {
    let table = part::scan(disk.as_ref())?;
    let mut out = Vec::new();
    for p in table.partitions {
        if let Some(idx) = only_part {
            if p.index != idx {
                continue;
            }
        } else if !p.probe_worthy {
            continue;
        }
        let dev: Box<dyn ReadAt> = Box::new(p.open(Arc::clone(disk)));
        match fsys::open(dev) {
            Ok(fs) => out.push((p, fs)),
            Err(e) => {
                if only_part.is_some() {
                    return Err(e.into());
                }
            }
        }
    }
    if out.is_empty() {
        bail!(
            "no readable filesystem found{}",
            only_part.map(|i| format!(" on partition {i}")).unwrap_or_default()
        );
    }
    Ok(out)
}

type PartScan = (Partition, Box<dyn FileSystem>, BootScan);

/// The partition whose boot entries we trust most: config-derived entries
/// (they carry the cmdline) beat fallback directory scans.
fn best_boot_scan(disk: &Arc<DiskImage>) -> Result<Option<PartScan>> {
    let has_config =
        |s: &BootScan| s.entries.iter().any(|e| e.source != Source::Fallback);
    let mut best: Option<PartScan> = None;
    for (p, fs) in open_filesystems(disk, None)? {
        let scan = lbx::boot::scan(fs.as_ref())?;
        if scan.entries.is_empty() {
            continue;
        }
        let better = match &best {
            None => true,
            Some((_, _, b)) => has_config(&scan) && !has_config(b),
        };
        if better {
            best = Some((p, fs, scan));
        }
    }
    Ok(best)
}

fn cmd_info(image: &str) -> Result<()> {
    let disk = open_disk(image)?;
    println!("format: {}", disk.format());
    println!("virtual size: {} ({} bytes)", human_size(disk.size()), disk.size());

    let table = part::scan(disk.as_ref())?;
    println!("partition table: {:?}", table.kind);
    for p in &table.partitions {
        let dev: Box<dyn ReadAt> = Box::new(p.open(Arc::clone(&disk)));
        let fs = fsys::detect(&dev)?.unwrap_or("-");
        println!(
            "  {:>3}  {:>10}  {:<22} {:<10} {:<38} {}",
            p.index,
            human_size(p.size_bytes),
            p.kind,
            fs,
            p.part_uuid.as_deref().unwrap_or("-"),
            p.name.as_deref().unwrap_or("")
        );
    }
    Ok(())
}

pub(crate) fn print_entry(part: usize, entry: &BootEntry, fixed: Option<&str>) {
    if let Some(kernel) = &entry.kernel {
        println!("      kernel:  {}", uri(part, kernel));
    }
    for initrd in &entry.initrd {
        println!("      initrd:  {}", uri(part, initrd));
    }
    if let Some(cmdline) = &entry.cmdline {
        println!("      cmdline: {cmdline}");
        if let Some(fixed) = fixed {
            println!("      vdafix:  {fixed}");
        } else if cmdline.contains("root=/dev/sd") || cmdline.contains("root=/dev/nvme") {
            println!("      note: root= names a physical device; under virtio this likely needs rewriting (e.g. /dev/vda)");
        }
    }
}

/// PARTUUID rewrite of this entry's cmdline, when one applies.
fn fixed_cmdline(entry: &BootEntry, table: &part::PartitionTable) -> Option<String> {
    lbx::vdafix::fix_cmdline(entry.cmdline.as_deref()?, table)
}

pub(crate) fn entry_label(entry: &BootEntry) -> &str {
    entry
        .title
        .as_deref()
        .or(entry.version.as_deref())
        .or(entry.id.as_deref())
        .unwrap_or("(untitled)")
}

/// The compression wrapping this entry's kernel (gzip `Image.gz` or EFI
/// zboot), sniffed from a header prefix, or `None` for an already-raw
/// kernel / x86 bzImage. A direct-kernel-boot caller uses it to decide
/// whether to extract the kernel with `--decompress`.
fn kernel_compression(fs: &dyn FileSystem, entry: &BootEntry) -> Option<lbx::kernel::Compression> {
    let kernel = entry.kernel.as_ref()?;
    let head = fs.read_prefix(kernel, lbx::kernel::SNIFF_LEN).ok()?;
    lbx::kernel::compression(&head)
}

fn cmd_boot_info(image: &str, json: bool) -> Result<()> {
    let disk = open_disk(image)?;
    let Some((p, fs, scan)) = best_boot_scan(&disk)? else {
        bail!("no boot artifacts (kernel/initramfs) found in any partition");
    };
    let index = scan.default.unwrap_or(0);
    let entry = &scan.entries[index];
    let table = part::scan(disk.as_ref())?;
    let fixed = fixed_cmdline(entry, &table);

    if json {
        println!(
            "{}",
            boot_info_json(p.index, entry, fixed.as_deref(), &scan.configs, fs.as_ref())
        );
        return Ok(());
    }
    let Some(kernel) = &entry.kernel else {
        bail!("default entry has no kernel path");
    };
    println!("kernel:  {}", uri(p.index, kernel));
    for initrd in &entry.initrd {
        println!("initrd:  {}", uri(p.index, initrd));
    }
    if let Some(cmdline) = &entry.cmdline {
        println!("cmdline: {cmdline}");
        if let Some(fixed) = &fixed {
            println!("vdafix:  {fixed}");
        }
    } else {
        println!("cmdline: (unknown — entry came from {} scan)", entry.source);
    }
    if let Some(c) = kernel_compression(fs.as_ref(), entry) {
        println!("compression: {} (extract with --decompress for direct-kernel boot)", c.label());
    }
    println!("source:  {}", entry.source);
    Ok(())
}

fn cmd_entries(image: &str, json: bool) -> Result<()> {
    let disk = open_disk(image)?;
    let table = part::scan(disk.as_ref())?;
    let mut found = false;
    let mut json_items: Vec<String> = Vec::new();

    for (p, fs) in open_filesystems(&disk, None)? {
        let scan = lbx::boot::scan(fs.as_ref())?;
        if scan.entries.is_empty() {
            continue;
        }
        found = true;
        if json {
            for (i, entry) in scan.entries.iter().enumerate() {
                let fixed = fixed_cmdline(entry, &table);
                json_items.push(entry_json(
                    p.index,
                    Some(i) == scan.default,
                    entry,
                    fixed.as_deref(),
                    fs.as_ref(),
                ));
            }
            continue;
        }
        println!("partition {} ({}):", p.index, fs.fs_type());
        for (i, entry) in scan.entries.iter().enumerate() {
            let mark = if Some(i) == scan.default { "*" } else { " " };
            println!("{mark} [{}] {} ({})", i + 1, entry_label(entry), entry.source);
            print_entry(p.index, entry, fixed_cmdline(entry, &table).as_deref());
            if let Some(c) = kernel_compression(fs.as_ref(), entry) {
                println!("      note: compressed kernel ({}); extract with --decompress for direct-kernel boot", c.label());
            }
            if lbx::boot::dma_restricted_pool(fs.as_ref(), entry) == Some(false) {
                println!("      note: kernel lacks CONFIG_DMA_RESTRICTED_POOL; virtio fails under a protected VM");
            }
        }
    }
    if json {
        println!("[{}]", json_items.join(","));
        return Ok(());
    }
    if !found {
        bail!("no boot entries found in any partition");
    }
    Ok(())
}

fn cmd_ls(image: &str, path: &str, part: Option<usize>) -> Result<()> {
    let disk = open_disk(image)?;
    let (uri_part, path) = parse_uri(path);
    let part = uri_part.or(part);
    for (p, fs) in open_filesystems(&disk, part)? {
        let Ok(entries) = fs.read_dir(&path) else {
            continue;
        };
        println!("# partition {} ({}):{}", p.index, fs.fs_type(), path);
        print_listing(entries);
        if part.is_none() {
            // Without an explicit partition we show the first match.
            return Ok(());
        }
    }
    Ok(())
}

pub(crate) fn print_listing(mut entries: Vec<DirEntry>) {
    entries.sort_by(|a, b| a.name.cmp(&b.name));
    for e in entries {
        let marker = match e.kind {
            FileKind::Dir => "/",
            FileKind::Symlink => "@",
            _ => "",
        };
        println!("{:>12}  {}{}", e.size, e.name, marker);
    }
}

/// Read `path` (or URI) from the partition it names, or the first
/// partition that has it.
fn read_by_uri(
    disk: &Arc<DiskImage>,
    path: &str,
    part: Option<usize>,
) -> Result<Vec<u8>> {
    let (uri_part, path) = parse_uri(path);
    let part = uri_part.or(part);
    for (_, fs) in open_filesystems(disk, part)? {
        if let Ok(data) = fs.read_file(&path) {
            return Ok(data);
        }
    }
    bail!(
        "{path}: not found{}",
        part.map(|i| format!(" on partition {i}")).unwrap_or(" on any readable partition".into())
    );
}

fn cmd_cat(image: &str, path: &str, part: Option<usize>) -> Result<()> {
    let disk = open_disk(image)?;
    let data = read_by_uri(&disk, path, part)?;
    std::io::stdout().write_all(&data)?;
    Ok(())
}

/// Copy into `dest`; an existing directory gets the source's basename.
pub(crate) fn write_dest(dest: &Path, source_path: &str, data: &[u8]) -> Result<PathBuf> {
    let dest = if dest.is_dir() {
        dest.join(source_path.rsplit('/').next().unwrap_or("file"))
    } else {
        dest.to_path_buf()
    };
    std::fs::write(&dest, data).with_context(|| format!("writing {}", dest.display()))?;
    Ok(dest)
}

fn cmd_cp(
    image: &str,
    source: &str,
    dest: &Path,
    part: Option<usize>,
    decompress: bool,
) -> Result<()> {
    let disk = open_disk(image)?;
    let mut data = read_by_uri(&disk, source, part)?;
    if decompress {
        data = lbx::kernel::to_bootable(data)?;
    }
    let (_, path) = parse_uri(source);
    let written = write_dest(dest, &path, &data)?;
    println!("{} -> {} ({})", source, written.display(), human_size(data.len() as u64));
    Ok(())
}

fn cmd_md5(image: &str, path: &str, part: Option<usize>, decompress: bool) -> Result<()> {
    let disk = open_disk(image)?;
    let mut data = read_by_uri(&disk, path, part)?;
    if decompress {
        data = lbx::kernel::to_bootable(data)?;
    }
    // md5sum-compatible "<hex>  <name>", so the output diffs cleanly
    // against `md5sum` run on an extracted copy.
    println!("{:x}  {}", md5::compute(&data), path);
    Ok(())
}

fn cmd_extract(
    image: &str,
    output: &Path,
    entry_arg: Option<usize>,
    dry_run: bool,
    vdafix: bool,
    decompress: bool,
) -> Result<()> {
    let disk = open_disk(image)?;
    let Some((p, fs, scan)) = best_boot_scan(&disk)? else {
        bail!("no boot artifacts (kernel/initramfs) found in any partition");
    };

    let index = match entry_arg {
        Some(n) if n >= 1 && n <= scan.entries.len() => n - 1,
        Some(n) => bail!("entry {n} out of range (1..={})", scan.entries.len()),
        None => scan.default.unwrap_or(0),
    };
    let entry = &scan.entries[index];
    let Some(kernel) = &entry.kernel else {
        bail!("selected entry has no kernel path");
    };
    let table = part::scan(disk.as_ref())?;
    let fixed = fixed_cmdline(entry, &table);

    println!("partition {} ({}), entry {} ({}):", p.index, fs.fs_type(), index + 1, entry.source);
    print_entry(p.index, entry, fixed.as_deref());
    for cfg in &scan.configs {
        println!("      config:  {}", uri(p.index, cfg));
    }
    if dry_run {
        return Ok(());
    }

    std::fs::create_dir_all(output)?;
    let copy = |fs_path: &str| -> Result<()> {
        let data = fs
            .read_file(fs_path)
            .with_context(|| format!("reading {fs_path}"))?;
        let dest = write_dest(output, fs_path, &data)?;
        println!("  -> {}", dest.display());
        Ok(())
    };
    // The kernel may be gzip/zboot-wrapped; only it gets decompressed (the
    // initrd must stay compressed — the guest kernel unpacks it itself).
    let kdata = fs.read_file(kernel).with_context(|| format!("reading {kernel}"))?;
    let kdata = if decompress { lbx::kernel::to_bootable(kdata)? } else { kdata };
    let kdest = write_dest(output, kernel, &kdata)?;
    println!("  -> {}", kdest.display());
    for initrd in &entry.initrd {
        copy(initrd)?;
    }
    for cfg in &scan.configs {
        copy(cfg)?;
    }
    let cmdline = match (vdafix, &fixed) {
        (true, Some(fixed)) => Some(fixed.as_str()),
        _ => entry.cmdline.as_deref(),
    };
    if let Some(cmdline) = cmdline {
        let dest = output.join("cmdline");
        std::fs::write(&dest, cmdline)?;
        println!("  -> {}", dest.display());
    }
    Ok(())
}

// --- minimal JSON emission (output only; not worth a serde dependency) ---

fn jstr(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn jopt(s: &Option<String>) -> String {
    s.as_ref().map(|s| jstr(s)).unwrap_or_else(|| "null".into())
}

fn jsize(n: Option<u64>) -> String {
    n.map(|n| n.to_string()).unwrap_or_else(|| "null".into())
}

fn entry_json(
    part: usize,
    is_default: bool,
    e: &BootEntry,
    fixed: Option<&str>,
    fs: &dyn FileSystem,
) -> String {
    // File sizes come from inode metadata only (no data read); a VMM
    // caching extracted files keys on URI + size to skip re-extraction.
    let kernel = e
        .kernel
        .as_ref()
        .map(|k| jstr(&uri(part, k)))
        .unwrap_or_else(|| "null".into());
    let kernel_size = jsize(e.kernel.as_ref().and_then(|k| fs.file_size(k).ok()));
    let initrd: Vec<String> = e.initrd.iter().map(|i| jstr(&uri(part, i))).collect();
    let initrd_size: Vec<String> =
        e.initrd.iter().map(|i| jsize(fs.file_size(i).ok())).collect();
    // Whether this kernel can do virtio in a gunyah protected VM (needs
    // CONFIG_DMA_RESTRICTED_POOL); null = could not determine.
    let dma_restricted_pool = lbx::boot::dma_restricted_pool(fs, e)
        .map(|b| b.to_string())
        .unwrap_or_else(|| "null".into());
    // Compression wrapping the kernel (gzip/zboot), so a direct-boot caller
    // knows to extract it with `--decompress`; null = already raw.
    let kernel_compression = kernel_compression(fs, e)
        .map(|c| jstr(&c.label()))
        .unwrap_or_else(|| "null".into());
    format!(
        "{{\"partition\":{part},\"default\":{is_default},\"source\":{},\"title\":{},\"version\":{},\"id\":{},\"kernel\":{kernel},\"kernel_size\":{kernel_size},\"initrd\":[{}],\"initrd_size\":[{}],\"cmdline\":{},\"cmdline_fixed\":{},\"dma_restricted_pool\":{dma_restricted_pool},\"kernel_compression\":{kernel_compression}}}",
        jstr(&e.source.to_string()),
        jopt(&e.title),
        jopt(&e.version),
        jopt(&e.id),
        initrd.join(","),
        initrd_size.join(","),
        jopt(&e.cmdline),
        fixed.map(jstr).unwrap_or_else(|| "null".into()),
    )
}

fn boot_info_json(
    part: usize,
    e: &BootEntry,
    fixed: Option<&str>,
    configs: &[String],
    fs: &dyn FileSystem,
) -> String {
    let entry = entry_json(part, true, e, fixed, fs);
    let configs: Vec<String> = configs.iter().map(|c| jstr(&uri(part, c))).collect();
    format!(
        "{{\"entry\":{entry},\"configs\":[{}]}}",
        configs.join(",")
    )
}

pub(crate) fn human_size(bytes: u64) -> String {
    const UNITS: &[&str] = &["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}
