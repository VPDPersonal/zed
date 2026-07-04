//! Pure, host-agnostic parsers for the files a Solution Explorer tree is built from.
//! Kept free of any gpui/fs dependency so the same logic can back a future WASM extension
//! (see the panel-views-extension architecture) and so it stays unit-testable on plain strings.

use quick_xml::Reader;
use quick_xml::events::Event;

/// A project entry referenced by a `.sln` solution file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SolutionProject {
    pub name: String,
    /// Worktree-relative path to the `.csproj`, normalized to `/` separators.
    pub csproj_rel: String,
}

/// Parses the `Project(...) = "Name", "path.csproj", "{guid}"` lines of a `.sln` file.
/// Solution folders (whose path is a plain name rather than a `.csproj`) are skipped.
pub fn parse_solution(text: &str) -> Vec<SolutionProject> {
    let mut projects = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        let Some(rest) = line.strip_prefix("Project(") else {
            continue;
        };
        // After the `=` come three quoted fields: name, path, project guid.
        let Some((_, after_equals)) = rest.split_once('=') else {
            continue;
        };
        let quoted = quoted_fields(after_equals);
        let (Some(name), Some(path)) = (quoted.first(), quoted.get(1)) else {
            continue;
        };
        let csproj_rel = normalize_separators(path);
        if !csproj_rel.to_ascii_lowercase().ends_with(".csproj") {
            continue;
        }
        projects.push(SolutionProject {
            name: name.to_string(),
            csproj_rel,
        });
    }
    projects
}

/// References extracted from a `.csproj` file.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CsprojInfo {
    /// Whether the project uses an MSBuild SDK (`<Project Sdk="...">`). SDK-style projects
    /// compile their directory tree via implicit globs; legacy projects (e.g. Unity-generated
    /// ones) list every file explicitly in [`Self::compile_items`].
    pub is_sdk_style: bool,
    /// NuGet package references as `(name, version)`; version is absent when not specified inline.
    pub package_references: Vec<(String, Option<String>)>,
    /// Worktree-relative (to the `.csproj`) paths of referenced projects, normalized to `/`.
    pub project_references: Vec<String>,
    /// Explicit `<Compile Include>`/`<None Include>` item paths (relative to the `.csproj`),
    /// normalized to `/`. Empty for SDK-style projects that rely on implicit globs.
    pub compile_items: Vec<String>,
}

/// Parses `PackageReference`/`ProjectReference`/`Compile`/`None` elements out of a `.csproj` file.
pub fn parse_csproj(text: &str) -> CsprojInfo {
    let mut info = CsprojInfo::default();
    let mut reader = Reader::from_str(text);
    reader.config_mut().check_end_names = false;
    loop {
        match reader.read_event() {
            Ok(Event::Eof) => break,
            Ok(Event::Start(element) | Event::Empty(element)) => {
                let tag = element.name();
                match tag.as_ref() {
                    b"Project" => {
                        if attribute_value(&element, b"Sdk").is_some() {
                            info.is_sdk_style = true;
                        }
                    }
                    b"PackageReference" => {
                        if let Some(name) = attribute_value(&element, b"Include") {
                            let version = attribute_value(&element, b"Version");
                            info.package_references.push((name, version));
                        }
                    }
                    b"ProjectReference" => {
                        if let Some(include) = attribute_value(&element, b"Include") {
                            info.project_references.push(normalize_separators(&include));
                        }
                    }
                    // `Include`-less `Compile`/`None` elements (`Remove`/`Update` operations) are skipped.
                    b"Compile" | b"None" => {
                        if let Some(include) = attribute_value(&element, b"Include") {
                            info.compile_items.push(normalize_separators(&include));
                        }
                    }
                    _ => {}
                }
            }
            Ok(_) => {}
            // A malformed `.csproj` yields whatever references were parsed so far rather than nothing.
            Err(_) => break,
        }
    }
    info
}

/// The relevant fields of a Unity `.asmdef` assembly definition file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AsmdefInfo {
    pub name: String,
    /// Referenced assembly names or GUID references (`GUID:...`), verbatim.
    pub references: Vec<String>,
}

/// Parses a Unity `.asmdef` (JSON). Returns `None` when the JSON is invalid or has no `name`.
pub fn parse_asmdef(text: &str) -> Option<AsmdefInfo> {
    let value: serde_json::Value = serde_json::from_str(text).ok()?;
    let name = value.get("name")?.as_str()?.to_string();
    let references = value
        .get("references")
        .and_then(|references| references.as_array())
        .map(|references| {
            references
                .iter()
                .filter_map(|reference| reference.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    Some(AsmdefInfo { name, references })
}

fn attribute_value(element: &quick_xml::events::BytesStart, key: &[u8]) -> Option<String> {
    element
        .try_get_attribute(key)
        .ok()
        .flatten()
        .and_then(|attribute| attribute.unescape_value().ok())
        .map(|value| value.into_owned())
}

/// Extracts the sequence of double-quoted substrings from a line.
fn quoted_fields(input: &str) -> Vec<String> {
    let mut fields = Vec::new();
    let mut chars = input.chars();
    while let Some(character) = chars.next() {
        if character != '"' {
            continue;
        }
        let mut field = String::new();
        for inner in chars.by_ref() {
            if inner == '"' {
                break;
            }
            field.push(inner);
        }
        fields.push(field);
    }
    fields
}

fn normalize_separators(path: &str) -> String {
    path.replace('\\', "/")
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn parses_solution_projects_and_skips_folders() {
        let sln = r#"
Microsoft Visual Studio Solution File, Format Version 12.00
Project("{FAE04EC0-301F-11D3-BF4B-00C04F79EFBC}") = "Assembly-CSharp", "Assembly-CSharp.csproj", "{111}"
Project("{2150E333-8FDC-42A3-9474-1A3956D46DE8}") = "SolutionFolder", "SolutionFolder", "{222}"
Project("{FAE04EC0-301F-11D3-BF4B-00C04F79EFBC}") = "Game", "src\Game\Game.csproj", "{333}"
Global
EndGlobal
"#;
        assert_eq!(
            parse_solution(sln),
            vec![
                SolutionProject {
                    name: "Assembly-CSharp".to_string(),
                    csproj_rel: "Assembly-CSharp.csproj".to_string(),
                },
                SolutionProject {
                    name: "Game".to_string(),
                    csproj_rel: "src/Game/Game.csproj".to_string(),
                },
            ]
        );
    }

    #[test]
    fn parses_csproj_references() {
        let csproj = r#"
<Project Sdk="Microsoft.NET.Sdk">
  <ItemGroup>
    <PackageReference Include="Newtonsoft.Json" Version="13.0.3" />
    <PackageReference Include="Serilog" />
    <ProjectReference Include="..\Core\Core.csproj" />
  </ItemGroup>
</Project>
"#;
        let info = parse_csproj(csproj);
        assert!(info.is_sdk_style);
        assert_eq!(
            info.package_references,
            vec![
                ("Newtonsoft.Json".to_string(), Some("13.0.3".to_string())),
                ("Serilog".to_string(), None),
            ]
        );
        assert_eq!(info.project_references, vec!["../Core/Core.csproj".to_string()]);
        assert!(info.compile_items.is_empty());
    }

    #[test]
    fn parses_legacy_csproj_compile_items() {
        let csproj = r#"
<Project ToolsVersion="4.0" xmlns="http://schemas.microsoft.com/developer/msbuild/2003">
  <ItemGroup>
    <Compile Include="Assets\Scripts\Player.cs" />
    <Compile Include="Assets\Scripts\Enemy.cs" />
    <None Include="Packages\tech.aspid.fasttools\package.json" />
    <Compile Remove="Assets\Scripts\Old.cs" />
    <Reference Include="UnityEngine" />
  </ItemGroup>
</Project>
"#;
        let info = parse_csproj(csproj);
        assert!(!info.is_sdk_style);
        assert_eq!(
            info.compile_items,
            vec![
                "Assets/Scripts/Player.cs".to_string(),
                "Assets/Scripts/Enemy.cs".to_string(),
                "Packages/tech.aspid.fasttools/package.json".to_string(),
            ]
        );
    }

    #[test]
    fn parses_asmdef() {
        let asmdef = r#"{ "name": "Game.Runtime", "references": ["GUID:abc123", "Core.Runtime"] }"#;
        assert_eq!(
            parse_asmdef(asmdef),
            Some(AsmdefInfo {
                name: "Game.Runtime".to_string(),
                references: vec!["GUID:abc123".to_string(), "Core.Runtime".to_string()],
            })
        );
        assert_eq!(parse_asmdef("not json"), None);
    }
}
