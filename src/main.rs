// SPDX-License-Identifier: Apache-2.0

use anyhow::Result;
use build::*;
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(author, version, about, long_about = None)]
struct Cmd {
    #[command(subcommand)]
    pub subcmd: Subcmd,
}

#[derive(Subcommand)]
enum Subcmd {
    Build(BuildArgs),
}

fn main() -> Result<()> {
    let cmd = Cmd::parse();

    let status = match cmd.subcmd {
        Subcmd::Build(args) => build::build(args),
    };

    if let Err(ref e) = status {
        eprintln!("ERROR: {e}");
        e.chain()
            .skip(1)
            .for_each(|cause| eprintln!("\tcause: {cause}"));
    }

    status
}

/// Subcommand to build a new EIF image.
pub mod build {
    use super::*;
    use anyhow::{Context, anyhow};
    use aws_nitro_enclaves_image_format::{
        defs::{EIF_HDR_ARCH_ARM64, EifBuildInfo, EifIdentityInfo},
        utils::EifBuilder,
    };
    use chrono::{DateTime, Utc};
    use clap::ValueEnum;
    use serde::{Deserialize, Serialize};
    use serde_json::Value;
    use sha2::{Digest, Sha384};
    use std::{
        fs::{self, OpenOptions},
        io::Write,
        path::{Path, PathBuf},
        process::Command,
        time::SystemTime,
    };
    use tempfile::NamedTempFile;

    #[derive(Clone, Debug, ValueEnum)]
    pub enum Arch {
        #[clap(name = "x86_64")]
        X86_64,
        #[clap(name = "aarch64")]
        Aarch64,
    }

    /// Arguments to configure the EIF file built for use in krun-awsnitro.
    #[derive(Parser)]
    pub(super) struct BuildArgs {
        /// Architecture the EIF is being built for.
        #[arg(long)]
        arch: Arch,
        /// Enclave kernel.
        #[arg(short, long)]
        kernel: PathBuf,
        /// Enclave kernel cmdline.
        #[arg(short, long, default_value = "/usr/share/krun-awsnitro/cmdline")]
        cmdline: PathBuf,
        /// krun-awsnitro init binary.
        #[arg(long, default_value = "/usr/share/krun-awsnitro/init")]
        init: PathBuf,
        /// NSM kernel module.
        #[arg(long, default_value = "/usr/share/krun-awsnitro/nsm.ko")]
        nsm: PathBuf,
        /// Path to write the krun-awsnitro initrd.
        #[arg(long, default_value = "/usr/share/krun-awsnitro/bootstrap-initrd.img")]
        initrd: PathBuf,
        /// Path to write the EIF image to.
        #[arg(
            short,
            long,
            default_value = "/usr/share/krun-awsnitro/krun-awsnitro.eif"
        )]
        path: PathBuf,
    }

    pub(super) fn build(args: BuildArgs) -> Result<()> {
        let build_info = build_info(&args)?;

        let cmdline = fs::read_to_string(&args.cmdline)
            .with_context(|| format!("unable to read cmdline from {}", args.cmdline.display()))?;

        let flags = match args.arch {
            Arch::X86_64 => 0,
            Arch::Aarch64 => EIF_HDR_ARCH_ARM64,
        };

        initrd(&args).context("unable to build initrd")?;

        let mut build = EifBuilder::new(
            &args.kernel,
            cmdline,
            None,
            Sha384::new(),
            flags,
            build_info,
        );

        build.add_ramdisk(Path::new(&format!("{}-initrd.img", args.initrd.display())));

        let mut output = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(args.path)
            .context("failed to create output file")?;

        build.write_to(&mut output);

        Ok(())
    }

    fn build_info(args: &BuildArgs) -> Result<EifIdentityInfo> {
        let kernel_name = {
            let path = format!("{}", args.kernel.display());

            let mut sub: Vec<String> = path.split('/').map(|s| s.to_string()).collect();

            sub.pop()
        }
        .context("unable to get kernel name for EIF build info")?;

        let datetime: DateTime<Utc> = SystemTime::now().into();
        let version = env!("CARGO_PKG_VERSION").to_string();

        Ok(EifIdentityInfo {
            img_name: "krun-awsnitro-eif".to_string(),
            img_version: "n/a".to_string(),
            build_info: EifBuildInfo {
                build_time: format!("{}", datetime),
                build_tool: "krun-awsnitro-eif-ctl".to_string(),
                build_tool_version: version,
                img_os: "n/a".to_string(),
                img_kernel: kernel_name,
            },
            docker_info: Value::Null,
            custom_info: Value::Null,
        })
    }

    #[derive(Debug, Serialize, Deserialize)]
    struct InitRdTemplate {
        files: (DirTemplate, FileTemplate, FileTemplate),
    }

    #[derive(Debug, Serialize, Deserialize)]
    struct FileTemplate {
        path: String,
        source: String,
        mode: String,
    }

    #[derive(Debug, Serialize, Deserialize)]
    struct DirTemplate {
        path: String,
        directory: bool,
        mode: String,
    }

    fn initrd(args: &BuildArgs) -> Result<()> {
        let cfg_file = initrd_cfg(args).context("unable to generate initrd config YAML")?;

        let output = Command::new("/home/ec2-user/blobs/linuxkit")
            .args([
                "build",
                "--name",
                &format!("{}", args.initrd.display()),
                "--format",
                "kernel+initrd-nogz",
                "--no-sbom",
                cfg_file.path().to_str().unwrap(),
            ])
            .output()
            .context("unable to generate initrd")?;

        if !output.status.success() {
            return Err(anyhow!("linuxkit failed to generate initrd"));
        }

        Ok(())
    }

    fn initrd_cfg(args: &BuildArgs) -> Result<NamedTempFile> {
        let template = {
            let dev = DirTemplate {
                path: String::from("dev"),
                directory: true,
                mode: String::from("0755"),
            };

            let init = FileTemplate {
                path: String::from("init"),
                source: format!("{}", args.init.display()),
                mode: String::from("0755"),
            };

            let nsm = FileTemplate {
                path: String::from("nsm.ko"),
                source: format!("{}", args.nsm.display()),
                mode: String::from("0755"),
            };

            InitRdTemplate {
                files: (dev, init, nsm),
            }
        };

        let yaml = serde_yaml::to_string(&template)
            .context("unable to generate initrd YAML configuration")?;

        let mut tmpfile = NamedTempFile::new().context("unable to generate named tempfile")?;

        tmpfile
            .write_all(yaml.as_bytes())
            .context("unable to write initrd YAML configuration to tempfile")?;

        Ok(tmpfile)
    }
}
