use std::{
    borrow::Cow,
    io::{BufReader, BufWriter, Write as _},
    path::{Path, PathBuf},
};

use anyhow::Context as _;
use clap::Parser;
use fs_err::File;
use memofs::Vfs;
use rbx_dom_weak::{InstanceBuilder, WeakDom};

use super::resolve_path;
use crate::{
    snapshot::{apply_patch_set, compute_patch_set, InstanceContext, InstanceSnapshot, RojoTree},
    snapshot_middleware::snapshot_from_vfs,
    Project,
};

/// Applies a Rojo project overtop the provided file as a 'patch'.
///
/// Roughly equivalent to opening the file in Studio, syncing with the
/// plugin, then saving it.
#[derive(Debug, Parser)]
pub struct PatchCommand {
    /// Path to the project to apply. Defaults to the current directory.
    #[clap(default_value = "")]
    pub project: PathBuf,

    /// Path to the input Roblox file.
    #[clap(long, short)]
    pub input: PathBuf,

    /// Path to output the patched file to.
    #[clap(long, short)]
    pub output: PathBuf,
}

impl PatchCommand {
    pub fn run(self) -> anyhow::Result<()> {
        let project_path = resolve_path(&self.project);
        let input_path = resolve_path(&self.input);
        let output_path = resolve_path(&self.output);

        let output_kind = FileKind::from_path(&output_path).with_context(|| {
            format!(
                "the patch {} is not a valid Roblox file type",
                input_path.display()
            )
        })?;

        log::trace!("Reading input file");
        let input_dom = FileKind::from_path(&input_path)
            .with_context(|| {
                format!(
                    "the patch {} is not a valid Roblox file type",
                    input_path.display()
                )
            })?
            .open_file(&input_path)?;

        log::trace!("Constructing in-memory filesystem");
        let vfs = Vfs::new_default();
        vfs.set_watch_enabled(false);

        let real_project_path = if Project::is_project_file(&project_path) {
            Cow::Borrowed(project_path.as_ref())
        } else {
            Cow::Owned(project_path.join("default.project.json"))
        };

        log::debug!("Loading project file from {}", project_path.display());

        let root_project = Project::load_exact(&vfs, &real_project_path, None)?;

        let root_ref = input_dom.root_ref();
        log::trace!("Constructing RojoTree from input dom");
        let mut tree = RojoTree::new(InstanceSnapshot::from_tree(input_dom, root_ref));

        let root_id = tree.get_root_id();

        let instance_context =
            InstanceContext::with_emit_legacy_scripts(root_project.emit_legacy_scripts);

        log::trace!("Generating snapshot of project");
        let snapshot = snapshot_from_vfs(&instance_context, &vfs, &real_project_path)?;

        log::trace!("Computing patch for project to input file");
        let patch_set = compute_patch_set(snapshot, &tree, root_id);

        log::trace!("Applying patch");
        apply_patch_set(&mut tree, patch_set);

        log::trace!("Writing finished model");
        write_model(tree, &output_path, output_kind)?;

        let file_name = output_path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("<invalid utf-8>");

        println!(
            "Patched {file_name} with project {}",
            root_project
                .name
                .expect("top-level projects should have their name set")
        );

        Ok(())
    }
}

/// The different kinds of file that this command can accept.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FileKind {
    /// An XML model file.
    Rbxmx,

    /// An XML place file.
    Rbxlx,

    /// A binary model file.
    Rbxm,

    /// A binary place file.
    Rbxl,
}

impl FileKind {
    fn from_path(output: &Path) -> Option<FileKind> {
        let extension = output.extension()?.to_str()?;

        match extension {
            "rbxlx" => Some(FileKind::Rbxlx),
            "rbxmx" => Some(FileKind::Rbxmx),
            "rbxl" => Some(FileKind::Rbxl),
            "rbxm" => Some(FileKind::Rbxm),
            _ => None,
        }
    }

    fn open_file(self, path: &Path) -> anyhow::Result<WeakDom> {
        let content = BufReader::new(File::open(path)?);
        match self {
            FileKind::Rbxl => rbx_binary::from_reader(content).with_context(|| {
                format!(
                    "Could not deserialize binary place file at {}",
                    path.display()
                )
            }),
            FileKind::Rbxlx => {
                rbx_xml::from_reader(content, xml_decode_config()).with_context(|| {
                    format!("Could not deserialize XML place file at {}", path.display())
                })
            }
            FileKind::Rbxm => {
                let temp_tree = rbx_binary::from_reader(content).with_context(|| {
                    format!(
                        "Could not deserialize binary place file at {}",
                        path.display()
                    )
                })?;

                process_model_dom(temp_tree)
            }
            FileKind::Rbxmx => {
                let temp_tree =
                    rbx_xml::from_reader(content, xml_decode_config()).with_context(|| {
                        format!("Could not deserialize XML model file at {}", path.display())
                    })?;
                process_model_dom(temp_tree)
            }
        }
    }
}

fn process_model_dom(dom: WeakDom) -> anyhow::Result<WeakDom> {
    let temp_children = dom.root().children();
    if temp_children.len() == 1 {
        let real_root = dom.get_by_ref(temp_children[0]).unwrap();
        let mut new_tree = WeakDom::new(InstanceBuilder::new(real_root.class));
        for (name, property) in &real_root.properties {
            new_tree
                .root_mut()
                .properties
                .insert(*name, property.to_owned());
        }

        let children = dom.clone_multiple_into_external(real_root.children(), &mut new_tree);
        for child in children {
            new_tree.transfer_within(child, new_tree.root_ref());
        }
        Ok(new_tree)
    } else {
        anyhow::bail!(
            "Rojo does not currently support models with more \
        than one Instance at the Root!"
        );
    }
}

#[profiling::function]
fn write_model(tree: RojoTree, output: &Path, output_kind: FileKind) -> anyhow::Result<()> {
    let root_id = tree.get_root_id();

    let mut file = BufWriter::new(File::create(output)?);

    match output_kind {
        FileKind::Rbxm => {
            rbx_binary::to_writer(&mut file, tree.inner(), &[root_id])?;
        }
        FileKind::Rbxl => {
            let root_instance = tree.get_instance(root_id).unwrap();
            let top_level_ids = root_instance.children();

            rbx_binary::to_writer(&mut file, tree.inner(), top_level_ids)?;
        }
        FileKind::Rbxmx => {
            // Model files include the root instance of the tree and all its
            // descendants.

            rbx_xml::to_writer(&mut file, tree.inner(), &[root_id], xml_encode_config())?;
        }
        FileKind::Rbxlx => {
            // Place files don't contain an entry for the DataModel, but our
            // WeakDom representation does.

            let root_instance = tree.get_instance(root_id).unwrap();
            let top_level_ids = root_instance.children();

            rbx_xml::to_writer(&mut file, tree.inner(), top_level_ids, xml_encode_config())?;
        }
    }

    file.flush()?;

    Ok(())
}

fn xml_encode_config() -> rbx_xml::EncodeOptions<'static> {
    rbx_xml::EncodeOptions::new().property_behavior(rbx_xml::EncodePropertyBehavior::WriteUnknown)
}

fn xml_decode_config() -> rbx_xml::DecodeOptions<'static> {
    rbx_xml::DecodeOptions::new().property_behavior(rbx_xml::DecodePropertyBehavior::ReadUnknown)
}
