//! Collects default property values by generated a place file every kind of
//! instance in it, then uses Roblox Studio to re-save it with default property
//! information encoded in it.

use std::{
    borrow::Cow,
    collections::{HashSet, VecDeque},
    convert::TryInto,
    fmt::{self, Write},
    fs::{self, File},
    io::BufReader,
    process::Command,
    sync::mpsc,
    time::Duration,
};

use notify::{DebouncedEvent, Watcher};
use rbx_dom_weak::{RbxTree, RbxValueType};
use rbx_reflection::{PropertyDescriptor, PropertyKind, PropertySerialization, ReflectionDatabase};
use roblox_install::RobloxStudio;
use tempfile::tempdir;

use crate::plugin_injector::{PluginInjector, StudioInfo};

/// Use Roblox Studio to populate the reflection database with default values
/// for as many properties as possible.
pub fn measure_default_properties(database: &mut ReflectionDatabase) -> anyhow::Result<()> {
    let fixture_place = generate_fixture_place(database);
    let output = roundtrip_place_through_studio(&fixture_place)?;

    database.version = output.info.version;

    apply_defaults_from_fixture_place(database, &output.tree);

    Ok(())
}

fn apply_defaults_from_fixture_place(database: &mut ReflectionDatabase, tree: &RbxTree) {
    // Perform a breadth-first search to find the instance shallowest in the
    // tree of each class.

    let mut found_classes = HashSet::new();
    let mut to_visit = VecDeque::new();

    let root_instance = tree.get_instance(tree.get_root_id()).unwrap();
    to_visit.extend(root_instance.get_children_ids());

    while let Some(id) = to_visit.pop_front() {
        let instance = tree.get_instance(id).unwrap();

        to_visit.extend(instance.get_children_ids());

        if found_classes.contains(&instance.class_name) {
            continue;
        }

        found_classes.insert(instance.class_name.clone());

        for (prop_name, prop_value) in &instance.properties {
            let descriptors = match find_descriptors(database, &instance.class_name, prop_name) {
                Some(descriptor) => descriptor,
                None => {
                    log::info!(
                        "Property {}.{} found in default place but not API dump",
                        instance.class_name,
                        prop_name
                    );
                    continue;
                }
            };

            match &descriptors.canonical.kind {
                PropertyKind::Canonical { serialization } => match serialization {
                    PropertySerialization::Serializes => {
                        if &descriptors.canonical.name != prop_name {
                            log::error!("Property {}.{} is supposed to serialize as {}, but was actually serialized as {}",
                                instance.class_name,
                                descriptors.canonical.name,
                                descriptors.canonical.name,
                                prop_name);
                        }
                    }

                    PropertySerialization::DoesNotSerialize => {
                        log::error!(
                            "Property {}.{} (canonical name {}) found in default place but should not serialize",
                            instance.class_name,
                            prop_name,
                            descriptors.canonical.name,
                        );
                    }

                    PropertySerialization::SerializesAs(serialized_name) => {
                        if serialized_name != prop_name {
                            log::error!("Property {}.{} is supposed to serialize as {}, but was actually serialized as {}",
                                instance.class_name,
                                descriptors.canonical.name,
                                serialized_name,
                                prop_name);
                        }
                    }

                    unknown => {
                        log::error!(
                            "Unknown property serialization {:?} on property {}.{}",
                            unknown,
                            instance.class_name,
                            descriptors.canonical.name
                        );
                    }
                },

                _ => panic!(
                    "find_descriptors must not return a non-canonical descriptor as canonical"
                ),
            }

            let canonical_name = Cow::Owned(descriptors.canonical.name.clone().into_owned());

            match prop_value.get_type() {
                // We don't support usefully emitting these types yet.
                RbxValueType::Ref | RbxValueType::SharedString => {}

                _ => {
                    let class_descriptor =
                        match database.classes.get_mut(instance.class_name.as_str()) {
                            Some(descriptor) => descriptor,
                            None => {
                                log::warn!(
                                    "Class {} found in default place but not API dump",
                                    instance.class_name
                                );
                                continue;
                            }
                        };

                    class_descriptor
                        .default_properties
                        .insert(canonical_name, prop_value.clone().try_into().unwrap());
                }
            }
        }
    }
}

struct Descriptors<'a> {
    input: &'a PropertyDescriptor<'a>,
    canonical: &'a PropertyDescriptor<'a>,
}

fn find_descriptors<'a>(
    database: &'a ReflectionDatabase,
    class_name: &str,
    prop_name: &str,
) -> Option<Descriptors<'a>> {
    let mut input_descriptor = None;
    let mut next_class_name = Some(class_name);

    while let Some(current_class_name) = next_class_name {
        let class = database.classes.get(current_class_name).unwrap();

        if let Some(prop) = class.properties.get(prop_name) {
            if input_descriptor.is_none() {
                input_descriptor = Some(prop);
            }

            match &prop.kind {
                PropertyKind::Canonical { .. } => {
                    return Some(Descriptors {
                        input: input_descriptor.unwrap(),
                        canonical: prop,
                    });
                }
                PropertyKind::Alias { alias_for } => {
                    let aliased_prop = class.properties.get(alias_for).unwrap();

                    return Some(Descriptors {
                        input: input_descriptor.unwrap(),
                        canonical: aliased_prop,
                    });
                }
                unknown => {
                    log::warn!("Unknown property kind {:?}", unknown);
                    return None;
                }
            }
        }

        next_class_name = class.superclass.as_ref().map(|name| name.as_ref());
    }

    None
}

struct StudioOutput {
    info: StudioInfo,
    tree: RbxTree,
}

/// Generate a new fixture place from the given reflection database, open it in
/// Studio, coax Studio to re-save it, and reads back the resulting place.
fn roundtrip_place_through_studio(place_contents: &str) -> anyhow::Result<StudioOutput> {
    let output_dir = tempdir()?;
    let output_path = output_dir.path().join("roundtrip.rbxlx");
    log::info!("Generating place at {}", output_path.display());
    fs::write(&output_path, place_contents)?;

    let studio_install = RobloxStudio::locate()?;
    let injector = PluginInjector::start(&studio_install);

    log::info!("Starting Roblox Studio...");

    let mut studio_process = Command::new(studio_install.application_path())
        .arg(output_path.display().to_string())
        .spawn()?;

    let info = injector.receive_info();

    let (tx, rx) = mpsc::channel();
    let mut watcher = notify::watcher(tx, Duration::from_millis(300))?;
    watcher.watch(&output_path, notify::RecursiveMode::NonRecursive)?;

    log::info!("Waiting for Roblox Studio to re-save place...");
    println!("Please save the opened place in Roblox Studio (ctrl+s).");

    // TODO: User currently has to manually save the place. We could use a crate
    // like enigo or maybe raw input calls to do this for them.

    loop {
        match rx.recv()? {
            DebouncedEvent::Write(_) => break,
            _ => {}
        }
    }

    log::info!("Place saved, killing Studio...");
    studio_process.kill()?;

    log::info!("Reading back place file...");

    let mut file = BufReader::new(File::open(output_path)?);

    let decode_options = rbx_xml::DecodeOptions::new()
        .property_behavior(rbx_xml::DecodePropertyBehavior::NoReflection);
    let tree = rbx_xml::from_reader(&mut file, decode_options)?;

    Ok(StudioOutput { info, tree })
}

/// Create a place file that contains a copy of every Roblox class and no
/// properties defined.
///
/// When this place is re-saved by Roblox Studio, it'll contain default values
/// for every property.
fn generate_fixture_place(database: &ReflectionDatabase) -> String {
    log::info!("Generating place with every instance...");

    let mut output = String::new();

    writeln!(&mut output, "<roblox version=\"4\">").unwrap();

    for descriptor in database.classes.values() {
        let mut instance = FixtureInstance::named(&descriptor.name);

        match &*descriptor.name {
            // These types can't be put into place files by default.
            "DebuggerWatch" | "DebuggerBreakpoint" | "AdvancedDragger" | "Dragger"
            | "ScriptDebugger" | "PackageLink" => continue,

            // rbx_xml does not currently support Ray values.
            // https://github.com/rojo-rbx/rbx-dom/issues/87
            "RayValue" => continue,

            // rbx_xml does not currently support Faces values.
            // https://github.com/rojo-rbx/rbx-dom/issues/88
            "Handles" => continue,

            // rbx_xml does not currently support Axes values.
            // https://github.com/rojo-rbx/rbx-dom/issues/89
            "ArcHandles" => continue,

            // These types have specific parenting restrictions handled
            // elsewhere.
            "Terrain"
            | "Attachment"
            | "Animator"
            | "StarterPlayerScripts"
            | "StarterCharacterScripts"
            | "Bone" => continue,

            // WorldModel is not yet enabled.
            "WorldModel" => continue,

            "StarterPlayer" => {
                instance.add_child(FixtureInstance::named("StarterPlayerScripts"));
                instance.add_child(FixtureInstance::named("StarterCharacterScripts"));
            }
            "Workspace" => {
                instance.add_child(FixtureInstance::named("Terrain"));
            }
            "Part" => {
                instance.add_child(FixtureInstance::named("Attachment"));
                instance.add_child(FixtureInstance::named("Bone"));
            }
            "Humanoid" => {
                instance.add_child(FixtureInstance::named("Animator"));
            }
            _ => {}
        }

        write!(output, "{}", instance).unwrap();
    }

    writeln!(&mut output, "</roblox>").unwrap();
    output
}

struct FixtureInstance<'a> {
    name: &'a str,
    children: Vec<FixtureInstance<'a>>,
}

impl<'a> FixtureInstance<'a> {
    fn named(name: &'a str) -> Self {
        Self {
            name,
            children: Vec::new(),
        }
    }

    fn add_child(&mut self, child: FixtureInstance<'a>) {
        self.children.push(child);
    }
}

impl fmt::Display for FixtureInstance<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        writeln!(
            formatter,
            "<Item class=\"{}\" reference=\"{}\">",
            &self.name, &self.name
        )?;

        for child in &self.children {
            write!(formatter, "{}", child)?;
        }

        writeln!(formatter, "</Item>")?;

        Ok(())
    }
}
