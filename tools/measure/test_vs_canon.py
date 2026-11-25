#!/usr/bin/env python3
# SPDX-License-Identifier: MIT OR Apache-2.0
"""Unit tests for tools/measure/vs_canon.py."""

from __future__ import annotations

from pathlib import Path
import sys
import tempfile
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parent))
import vs_canon as vc  # noqa: E402

NS = "http://schemas.microsoft.com/developer/msbuild/2003"


def _write_vcxproj(path: Path, guid: str, config_type: str, extra_item_groups: str = "") -> None:
    path.write_text(
        f'<?xml version="1.0" encoding="utf-8"?>\n'
        f'<Project DefaultTargets="Build" ToolsVersion="17.0" xmlns="{NS}">\n'
        f'  <ItemGroup Label="ProjectConfigurations">\n'
        f'    <ProjectConfiguration Include="Debug|x64">\n'
        f'      <Configuration>Debug</Configuration>\n'
        f'      <Platform>x64</Platform>\n'
        f'    </ProjectConfiguration>\n'
        f'  </ItemGroup>\n'
        f'  <PropertyGroup Label="Globals">\n'
        f'    <ProjectGuid>{{{guid}}}</ProjectGuid>\n'
        f'  </PropertyGroup>\n'
        f'  <Import Project="$(VCTargetsPath)\\Microsoft.Cpp.Default.props" />\n'
        f'  <PropertyGroup Condition="\'$(Configuration)|$(Platform)\'==\'Debug|x64\'" Label="Configuration">\n'
        f'    <ConfigurationType>{config_type}</ConfigurationType>\n'
        f'  </PropertyGroup>\n'
        f'  <Import Project="$(VCTargetsPath)\\Microsoft.Cpp.props" />\n'
        f'  <ItemDefinitionGroup Condition="\'$(Configuration)|$(Platform)\'==\'Debug|x64\'">\n'
        f'    <ProjectReference>\n'
        f'      <LinkLibraryDependencies>false</LinkLibraryDependencies>\n'
        f'    </ProjectReference>\n'
        f'  </ItemDefinitionGroup>\n'
        f'{extra_item_groups}'
        f'  <Import Project="$(VCTargetsPath)\\Microsoft.Cpp.targets" />\n'
        f'</Project>\n',
        encoding="utf-8",
    )


class VsCanonTests(unittest.TestCase):
    def test_ignores_item_definition_group_project_reference_metadata(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            src = root / "src"
            build = root / "build"
            src.mkdir()
            build.mkdir()
            (src / "lib.c").write_text("int f(void) { return 0; }\n", encoding="utf-8")
            (src / "main.c").write_text("int main(void) { return 0; }\n", encoding="utf-8")

            lib_guid = "11111111-1111-1111-1111-111111111111"
            app_guid = "22222222-2222-2222-2222-222222222222"
            _write_vcxproj(
                build / "lib.vcxproj",
                lib_guid,
                "StaticLibrary",
                f'  <ItemGroup>\n    <ClCompile Include="{src / "lib.c"}" />\n  </ItemGroup>\n',
            )
            _write_vcxproj(
                build / "app.vcxproj",
                app_guid,
                "Application",
                f'  <ItemGroup>\n    <ClCompile Include="{src / "main.c"}" />\n  </ItemGroup>\n'
                f'  <ItemGroup>\n'
                f'    <ProjectReference Include="{build / "lib.vcxproj"}">\n'
                f'      <Project>{{{lib_guid}}}</Project>\n'
                f'    </ProjectReference>\n'
                f'  </ItemGroup>\n',
            )
            (build / "demo.sln").write_text(
                "Microsoft Visual Studio Solution File, Format Version 12.00\n"
                "# Visual Studio Version 17\n"
                f'Project("{{8BC9CEB8-8B4A-11D0-8D11-00A0C91BC942}}") = "lib", "lib.vcxproj", "{{{lib_guid}}}"\n'
                "EndProject\n"
                f'Project("{{8BC9CEB8-8B4A-11D0-8D11-00A0C91BC942}}") = "app", "app.vcxproj", "{{{app_guid}}}"\n'
                "EndProject\n"
                "Global\n"
                "\tGlobalSection(SolutionConfigurationPlatforms) = preSolution\n"
                "\t\tDebug|x64 = Debug|x64\n"
                "\tEndGlobalSection\n"
                "\tGlobalSection(ProjectConfigurationPlatforms) = postSolution\n"
                f"\t\t{{{lib_guid}}}.Debug|x64.ActiveCfg = Debug|x64\n"
                f"\t\t{{{app_guid}}}.Debug|x64.ActiveCfg = Debug|x64\n"
                "\tEndGlobalSection\n"
                "EndGlobal\n",
                encoding="utf-8",
            )

            result = vc.validate(vc.solution_files(build), build, source=src)
            self.assertEqual(result["projects"]["app.vcxproj"]["dependencies"], ["lib.vcxproj"])
            self.assertEqual(result["projects"]["app.vcxproj"]["sources"], ["main.c"])
            self.assertEqual(result["projects"]["lib.vcxproj"]["sources"], ["lib.c"])

    def test_rejects_item_group_project_reference_without_include(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            build = Path(tmp)
            proj = build / "bad.vcxproj"
            _write_vcxproj(
                proj,
                "33333333-3333-3333-3333-333333333333",
                "Application",
                '  <ItemGroup>\n    <ProjectReference><Project>{11111111-1111-1111-1111-111111111111}</Project></ProjectReference>\n  </ItemGroup>\n',
            )
            with self.assertRaisesRegex(ValueError, "Missing ProjectReference Include"):
                vc.validate([proj], build)


if __name__ == "__main__":
    unittest.main()
