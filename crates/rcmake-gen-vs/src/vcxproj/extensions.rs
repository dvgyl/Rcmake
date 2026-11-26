// SPDX-License-Identifier: MIT OR Apache-2.0

use rcmake_base::list;
use rcmake_host::{Host, ReadRange, path::to_native};
use rcmake_resolve::{ide::IdeLinkItem, usage::UsageResolver};

use crate::cuda::{self, Assembler, CompileMode, ImportKind, OptionText, SourceOptions};

use super::*;

#[derive(Default)]
pub(super) struct CudaConfig {
    includes: Vec<String>,
    defines: Vec<String>,
    options: String,
    architectures: Vec<String>,
    device_options: String,
    runtime: Option<String>,
    separable: bool,
    mode: Option<CompileMode>,
    device_link: bool,
    dependencies: Vec<String>,
}

impl<'a> Context<'a> {
    pub(crate) fn write_tool_imports(
        &self,
        host: &dyn Host,
        plan: &Plan,
        nasm_props_template: &mut Option<String>,
    ) -> Result<(), GenError> {
        if plan.configs.values().any(|config| config.cuda.is_some())
            && self.settings.cuda_version.is_none()
        {
            return Err(GenError::Invalid(
                "The Visual Studio CUDA toolkit version is missing.".into(),
            ));
        }
        if !plan.items.values().any(|item| item.kind == Kind::Nasm) {
            return Ok(());
        }
        let source = match nasm_props_template {
            Some(cached) => cached.as_str(),
            None => {
                let templates = self.settings.nasm_templates.as_ref().ok_or_else(|| {
                    GenError::Invalid("Cannot find the Visual Studio NASM templates.".into())
                })?;
                let path = join(templates, "nasm.props.in");
                let bytes = host
                    .read(
                        &to_native(path.as_bytes()),
                        ReadRange::All { limit: 64 * 1024 },
                    )
                    .map_err(|error| {
                        GenError::Invalid(format!("Cannot read {path}. {error}").into())
                    })?;
                let text = String::from_utf8(bytes).map_err(|error| {
                    GenError::Invalid(format!("Cannot read {path}. {error}").into())
                })?;
                nasm_props_template.insert(text).as_str()
            }
        };
        let compiler = self
            .definition(b"CMAKE_ASM_NASM_COMPILER")
            .map_or_else(|| "nasm".into(), |value| string(value));
        let mut escaped = String::new();
        crate::xml::escape(&mut escaped, &msbuild(&windows(&compiler)), false);
        let props = source.replace("@CMAKE_ASM_NASM_COMPILER@", &escaped);
        crate::write(
            host,
            &join(&self.nasm_props_dir(), "nasm.props"),
            props.as_bytes(),
        )
    }

    pub(super) fn tool_imports(&self, xml: &mut crate::xml::Writer, plan: &Plan, kind: ImportKind) {
        if plan.configs.values().any(|config| config.cuda.is_some())
            && let Some(version) = &self.settings.cuda_version
        {
            cuda::cuda_import(
                xml,
                kind,
                version,
                self.settings.cuda_integration.as_deref(),
            );
        }
        if plan.items.values().any(|item| item.kind == Kind::Masm) {
            cuda::masm_import(xml, kind);
        }
        if plan.items.values().any(|item| item.kind == Kind::Nasm)
            && let Some(templates) = &self.settings.nasm_templates
        {
            cuda::nasm_import(xml, kind, &self.nasm_props_dir(), templates);
        }
    }

    fn nasm_props_dir(&self) -> String {
        join(&self.project.dir, &format!("{}.dir", self.project.name))
    }

    pub(super) fn definition(&self, name: &[u8]) -> Option<&rcmake_base::BString> {
        let dir = self
            .project
            .target
            .and_then(|id| self.model.target(id))
            .map_or(rcmake_model::DirId::ROOT, |target| target.dir);
        self.model.definition(dir, name)
    }

    pub(super) fn fallback_source_kind(&self, path: &str, default: Kind) -> Kind {
        let tag = source_kind(path, default);
        if path
            .rsplit_once('.')
            .is_some_and(|(_, ext)| ext.eq_ignore_ascii_case("asm"))
        {
            if self.definition(b"CMAKE_ASM_MASM_COMPILER").is_some() {
                return Kind::Masm;
            }
            if self.definition(b"CMAKE_ASM_NASM_COMPILER").is_some() {
                return Kind::Nasm;
            }
        }
        tag
    }

    pub(super) fn toolchain_settings(
        &self,
        plan: &Plan,
        config: &mut Config,
        fact: Option<&ResolvedTarget>,
        name: &str,
        usage: &mut Option<UsageResolver<'a>>,
    ) -> Result<(), GenError> {
        let Some(id) = self.project.target else {
            return Ok(());
        };
        let check_msvc_runtime = self
            .definition(b"CMAKE_MSVC_RUNTIME_LIBRARY_DEFAULT")
            .is_some_and(|value| !value.is_empty());
        let mut compiled_c = false;
        let mut compiled_cxx = false;
        let mut own_cuda = false;
        for item in plan.items.values() {
            if item.kind == Kind::Compile && check_msvc_runtime {
                if let Some(source) = item.configs.get(name).filter(|source| !source.excluded) {
                    if source.language == "C" {
                        compiled_c = true;
                    } else if source.language == "CXX" {
                        compiled_cxx = true;
                    }
                }
            } else if item.kind == Kind::Cuda
                && item
                    .configs
                    .get(name)
                    .is_some_and(|source| !source.excluded)
            {
                own_cuda = true;
            }
        }
        if check_msvc_runtime {
            for (compiled, language) in [(compiled_c, b"C".as_slice()), (compiled_cxx, b"CXX")] {
                if compiled {
                    usage
                        .get_or_insert_with(|| UsageResolver::new(self.model))
                        .validate_msvc_runtime(id, name.as_bytes(), language)
                        .map_err(gen_error)?;
                }
            }
        }
        let mut dependencies = Vec::new();
        for item in fact
            .and_then(|fact| fact.ide.as_deref())
            .into_iter()
            .flat_map(|ide| &ide.link_items)
        {
            let IdeLinkItem::Target(id) = item else {
                continue;
            };
            let Some(target) = self
                .model
                .target(*id)
                .filter(|target| target.kind == TargetKind::StaticLibrary)
            else {
                continue;
            };
            if target
                .props
                .get(b"CUDA_SEPARABLE_COMPILATION")
                .is_some_and(|value| truth::is_on(value))
                && !target
                    .props
                    .get(b"CUDA_RESOLVE_DEVICE_SYMBOLS")
                    .is_some_and(|value| truth::is_on(value))
                && let Some(file) = self.target_library(*id, name)
            {
                dependencies.push(file);
            }
        }
        if !own_cuda && dependencies.is_empty() {
            return Ok(());
        }
        let resolver = usage.get_or_insert_with(|| UsageResolver::new(self.model));
        let mut property = |key, default| {
            resolver
                .target_toolchain_property(id, name.as_bytes(), b"CUDA", key, default)
                .map_err(gen_error)
        };
        let separable = own_cuda
            && property(b"CUDA_SEPARABLE_COMPILATION", None)?
                .is_some_and(|value| truth::is_on(&value));
        let device_link = property(b"CUDA_RESOLVE_DEVICE_SYMBOLS", None)?.map_or(
            (separable || !dependencies.is_empty())
                && matches!(
                    self.project.kind,
                    TargetKind::Executable | TargetKind::SharedLibrary | TargetKind::ModuleLibrary
                ),
            |value| truth::is_on(&value),
        );
        let runtime = if let Some(default) = self
            .definition(b"CMAKE_CUDA_RUNTIME_LIBRARY_DEFAULT")
            .filter(|value| !value.is_empty())
        {
            property(b"CUDA_RUNTIME_LIBRARY", Some(default))?.map(|value| string(&value))
        } else {
            None
        };
        let architecture = property(
            b"CUDA_ARCHITECTURES",
            self.definition(b"CMAKE_CUDA_ARCHITECTURES"),
        )?;
        let flags = |device| {
            rcmake_resolve::lower::cuda_ide_flags(
                self.model,
                id,
                name.as_bytes(),
                architecture.as_ref(),
                device,
            )
            .map(|flags| flags.iter().map(|flag| string(flag)).collect::<Vec<_>>())
            .map_err(gen_error)
        };
        let (architectures, device_options) =
            if self.definition(b"CMAKE_CUDA_COMPILER_ID").is_some() {
                (
                    flags(false)?,
                    if device_link {
                        flags(true)?.join(" ")
                    } else {
                        String::new()
                    },
                )
            } else {
                let flags =
                    architecture.map_or_else(Vec::new, |value| self.architecture_flags(&value));
                let options = flags.join(" ");
                (flags, options)
            };
        let compile = fact.and_then(|fact| fact.ide.as_deref()).and_then(|ide| {
            ide.compile
                .iter()
                .find(|compile| compile.language == b"CUDA")
        });
        let (includes, defines, options) = compile_values(compile);
        let mut mode = None;
        for (key, nvcc, extension) in [
            (b"CUDA_PTX_COMPILATION".as_slice(), "ptx", "ptx"),
            (b"CUDA_CUBIN_COMPILATION", "cubin", "cubin"),
            (b"CUDA_FATBIN_COMPILATION", "fatbin", "fatbin"),
            (b"CUDA_OPTIX_COMPILATION", "optix-ir", "optixir"),
        ] {
            if property(key, None)?.is_some_and(|value| truth::is_on(&value)) {
                mode = Some(CompileMode { nvcc, extension });
                break;
            }
        }
        config.cuda = Some(CudaConfig {
            includes,
            defines,
            options: options.join(" "),
            architectures,
            device_options,
            runtime,
            separable,
            mode,
            device_link,
            dependencies,
        });
        Ok(())
    }

    fn architecture_flags(&self, value: &[u8]) -> Vec<String> {
        if truth::is_off(value) {
            return Vec::new();
        }
        let special = match value {
            b"all" => Some(b"CMAKE_CUDA_ARCHITECTURES_ALL".as_slice()),
            b"all-major" => Some(b"CMAKE_CUDA_ARCHITECTURES_ALL_MAJOR".as_slice()),
            b"native" => Some(b"CMAKE_CUDA_ARCHITECTURES_NATIVE".as_slice()),
            _ => None,
        };
        let value = if let Some(key) = special {
            let Some(expanded) = self.definition(key).filter(|value| !value.is_empty()) else {
                return vec![format!("-arch={}", string(value))];
            };
            expanded.as_slice()
        } else {
            value
        };
        list::split(value, list::Empty::Drop)
            .iter()
            .map(|item| {
                let (arch, code) = if let Some(arch) = item.strip_suffix(b"-real") {
                    (string(arch), "real")
                } else if let Some(arch) = item.strip_suffix(b"-virtual") {
                    (string(arch), "virtual")
                } else {
                    (string(item), "")
                };
                let code = match code {
                    "real" => format!("sm_{arch}"),
                    "virtual" => format!("compute_{arch}"),
                    _ => format!("[compute_{arch},sm_{arch}]"),
                };
                format!("--generate-code=arch=compute_{arch},code={code}")
            })
            .collect()
    }

    pub(super) fn write_cuda_config(&self, xml: &mut crate::xml::Writer, config: &CudaConfig) {
        cuda::compile_options(
            xml,
            &cuda::CompileOptions {
                platform: &self.settings.platform,
                runtime: config.runtime.as_deref(),
                separable: config.separable,
                mode: config.mode,
                includes: &config.includes,
                defines: &config.defines,
                architecture_flags: &config.architectures,
                additional_options: OptionText::Literal(&config.options),
                ..Default::default()
            },
        );
        cuda::link_options(
            xml,
            &cuda::LinkOptions {
                target_kind: Some(self.project.kind),
                perform_device_link: config.device_link,
                dependencies: &config.dependencies,
                additional_options: OptionText::Literal(&config.device_options),
                ..Default::default()
            },
        );
    }

    pub(super) fn write_tool_item(&self, xml: &mut crate::xml::Writer, item: &Item, plan: &Plan) {
        for (name, config) in &item.configs {
            if config.excluded {
                continue;
            }
            let tool = match item.kind {
                Kind::Masm => Some((Assembler::Masm, b"ASM_MASM".as_slice())),
                Kind::Nasm => Some((Assembler::Nasm, b"ASM_NASM".as_slice())),
                _ => None,
            };
            let target_compile = tool.and_then(|(_, language)| {
                self.facts
                    .and_then(|facts| facts.get(name))
                    .and_then(|fact| fact.ide.as_deref())
                    .and_then(|ide| {
                        ide.compile
                            .iter()
                            .find(|compile| compile.language == language)
                    })
            });
            let (mut includes, mut defines, mut options) = compile_values(target_compile);
            let (source_includes, source_defines, source_options) =
                compile_values(config.tool_compile.as_ref());
            includes.extend(source_includes);
            defines.extend(source_defines);
            options.extend(source_options);
            let nasm = item.kind == Kind::Nasm;
            let nasm_output_switch = if nasm {
                let format = self
                    .definition(b"CMAKE_ASM_NASM_OBJECT_FORMAT")
                    .map_or_else(
                        || {
                            if self.settings.platform == "Win32" {
                                "win32".into()
                            } else {
                                "win64".into()
                            }
                        },
                        |value| string(value),
                    );
                Some(match format.as_str() {
                    "win32" => "0",
                    "win64" => "1",
                    "elf32" => "2",
                    "elf64" => "3",
                    _ => {
                        options.insert(0, format!("-f {}", rules::quote(&format)));
                        ""
                    }
                })
            } else {
                None
            };
            let output = if item.kind == Kind::Cuda
                && let Some(mode) = plan
                    .configs
                    .get(name)
                    .and_then(|config| config.cuda.as_ref()?.mode)
                && let Some((stem, _)) = config.object.rsplit_once('.')
            {
                windows(&format!("{stem}.{}", mode.extension))
            } else {
                windows(&config.object)
            };
            let flags = options.join(" ");
            let condition = self.settings.condition(name);
            let source = SourceOptions {
                path: &item.path,
                condition: Some(&condition),
                includes: &includes,
                defines: &defines,
                additional_options: OptionText::Literal(&flags),
                nasm_output: (nasm && !output.is_empty()).then_some(OptionText::Literal(&output)),
                object_output: (!nasm && !output.is_empty()).then_some(output.as_str()),
                nasm_output_switch,
                ..Default::default()
            };
            if let Some((assembler, _)) = tool {
                cuda::assembler_item(xml, assembler, &source);
            } else {
                cuda::cuda_item(xml, &source);
            }
        }
    }
}

fn gen_error(error: rcmake_resolve::ResolveError) -> GenError {
    GenError::Invalid(error.to_string().into())
}

fn compile_values(
    settings: Option<&IdeCompileSettings>,
) -> (Vec<String>, Vec<String>, Vec<String>) {
    let Some(settings) = settings else {
        return Default::default();
    };
    (
        settings
            .includes
            .iter()
            .map(|include| string(&include.path))
            .collect(),
        settings
            .defines
            .iter()
            .map(|define| string(&define.value))
            .collect(),
        settings
            .flags
            .iter()
            .flat_map(|flag| ide_flags(&string(&flag.value)))
            .map(|flag| rules::quote(&flag))
            .collect(),
    )
}
