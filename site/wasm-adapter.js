/* rcmake Browser WebAssembly Adapter & Playground Examples.
 *
 * Contract for the standalone WASI bundle at `./wasm/rcmake_wasm.js`:
 *   - `./wasm/rcmake_wasm.js` loads the standalone `wasm32-wasip1` command module
 *     `./wasm/rcmake_wasm.wasm` with a minimal in-browser WASI shim (stdin, stdout,
 *     stderr, clocks, and random bytes; no host filesystem access).
 *   - Default export `init(wasmUrl?)` compiles `./wasm/rcmake_wasm.wasm`.
 *   - Named export `configure_json(requestJson: string): string` sends a JSON request
 *     with `"op": "configure"` on WASI stdin to the PureHost configure engine:
 *       { files: Record<string, string>, generator: string, buildType: string, preset?: string, defines?: Record<string, string> }
 *     and reads JSON from stdout:
 *       { ok: boolean, durationMs?: number, diagnostics: Array<Diagnostic>, targets: Array<Target>, files: Record<string, string>, explainSymbols?: Array<string> }
 *   - Named export `explain_json(requestJson: string): string` sends `"op": "explain"` on stdin:
 *       { files: Record<string, string>, symbol: string, generator: string, buildType: string, config?: string }
 *     and returns an `rcmake-explain-v1` JSON object matching `crates/rcmake-api/src/explain_json.rs`.
 *
 * There is no precomputed fallback. If the WebAssembly runtime cannot load,
 * `RcmakeWasmAdapter` surfaces the load error directly.
 *
 * Licensed under MIT OR Apache-2.0.
 */

export const WASM_ENGINE_COMMIT = "ced2709";

export const PLAYGROUND_EXAMPLES = {
  "hello-cpp20": {
    id: "hello-cpp20",
    title: "Hello C++20",
    description: "C++20 executable with compile features, warnings, definitions, and strict diagnostic checks.",
    defaultSymbol: "hello",
    files: {
      "CMakeLists.txt": `cmake_minimum_required(VERSION 3.25...4.5)
project(HelloCpp20 VERSION 1.0.0 LANGUAGES CXX)

cmake_diagnostic(SET CMD_STRICT WARN)

add_executable(hello
  src/main.cpp
  src/greeting.cpp
)

target_compile_features(hello PRIVATE cxx_std_20)
target_compile_definitions(hello PRIVATE
  HELLO_VERSION="\${PROJECT_VERSION}"
  $<$<CONFIG:Debug>:HELLO_TRACE=1>
)
target_compile_options(hello PRIVATE
  $<$<CXX_COMPILER_ID:Clang,AppleClang,GNU>:-Wall -Wextra -Wpedantic>
  $<$<CXX_COMPILER_ID:MSVC>:/W4>
)
`
    }
  },

  "library-export": {
    id: "library-export",
    title: "Library + Export",
    description: "STATIC library with PUBLIC HEADERS file set, ALIAS target, install export set, and consumer executable.",
    defaultSymbol: "demo_cli",
    files: {
      "CMakeLists.txt": `cmake_minimum_required(VERSION 3.28...4.5)
project(GeometryKit VERSION 2.4.0 LANGUAGES CXX)

include(GNUInstallDirs)

add_library(geom STATIC)
add_library(GeometryKit::geom ALIAS geom)

target_sources(geom
  PRIVATE
    src/polygon.cpp
  PUBLIC
    FILE_SET HEADERS
    BASE_DIRS include
    FILES include/geom/polygon.hpp
)
target_compile_features(geom PUBLIC cxx_std_20)
target_compile_definitions(geom PUBLIC GEOM_API_VERSION=2)

add_executable(demo_cli app/cli.cpp)
target_link_libraries(demo_cli PRIVATE GeometryKit::geom)

install(TARGETS geom
  EXPORT GeometryKitTargets
  FILE_SET HEADERS DESTINATION \${CMAKE_INSTALL_INCLUDEDIR}
)
install(EXPORT GeometryKitTargets
  NAMESPACE GeometryKit::
  DESTINATION \${CMAKE_INSTALL_LIBDIR}/cmake/GeometryKit
)
`
    }
  },

  "genex": {
    id: "genex",
    title: "Generator Expressions",
    description: "Nested $<BUILD_INTERFACE>, $<INSTALL_INTERFACE>, $<CONFIG>, and $<COMPILE_LANG_AND_ID> evaluation.",
    defaultSymbol: "codec_app",
    files: {
      "CMakeLists.txt": `cmake_minimum_required(VERSION 3.26...4.5)
project(GenexShowcase LANGUAGES C CXX)

add_library(codec_flags INTERFACE)
target_include_directories(codec_flags INTERFACE
  $<BUILD_INTERFACE:\${CMAKE_CURRENT_SOURCE_DIR}/include>
  $<INSTALL_INTERFACE:include>
)
target_compile_definitions(codec_flags INTERFACE
  CODEC_BUILD_CONFIG="$<CONFIG>"
  $<$<BOOL:$<CONFIG:Debug>>:CODEC_ASSERTIONS=1>
  $<$<NOT:$<CONFIG:Debug>>:CODEC_FAST_PATH=1>
)
target_compile_options(codec_flags INTERFACE
  $<$<COMPILE_LANG_AND_ID:CXX,Clang,AppleClang,GNU>:-fno-omit-frame-pointer>
  $<$<COMPILE_LANG_AND_ID:C,Clang,AppleClang,GNU>:-Wstrict-prototypes>
)

add_executable(codec_app src/codec.cpp src/crc.c)
target_link_libraries(codec_app PRIVATE codec_flags)
`
    }
  },

  "presets-v13": {
    id: "presets-v13",
    title: "Presets v13",
    description: "CMakePresets.json schema v13 inheritance with ${sourceDir} and ${presetName} macro expansion and Ninja Multi-Config.",
    defaultSymbol: "CMAKE_EXPORT_COMPILE_COMMANDS",
    files: {
      "CMakeLists.txt": `cmake_minimum_required(VERSION 3.30...4.5)
project(PresetsV13Demo VERSION 0.9.0 LANGUAGES CXX)

option(DEMO_ENABLE_BENCH "Build benchmark runner" OFF)

add_library(engine STATIC src/engine.cpp)
target_compile_features(engine PUBLIC cxx_std_23)

add_executable(runner src/runner.cpp)
target_link_libraries(runner PRIVATE engine)

if(DEMO_ENABLE_BENCH)
  add_executable(bench src/bench.cpp)
  target_link_libraries(bench PRIVATE engine)
endif()
`,
      "CMakePresets.json": `{
  "version": 13,
  "cmakeMinimumRequired": { "major": 4, "minor": 5, "patch": 0 },
  "configurePresets": [
    {
      "name": "base",
      "hidden": true,
      "generator": "Ninja Multi-Config",
      "binaryDir": "\${sourceDir}/out/build/\${presetName}",
      "cacheVariables": {
        "CMAKE_EXPORT_COMPILE_COMMANDS": "ON",
        "DEMO_ENABLE_BENCH": "ON"
      }
    },
    {
      "name": "ci-release",
      "inherits": "base",
      "displayName": "CI Release (Presets v13)",
      "cacheVariables": {
        "CMAKE_CONFIGURATION_TYPES": "Debug;Release;RelWithDebInfo"
      }
    }
  ],
  "buildPresets": [
    {
      "name": "ci-release-build",
      "configurePreset": "ci-release",
      "configuration": "Release",
      "targets": ["runner", "bench"]
    }
  ],
  "workflowPresets": [
    {
      "name": "ci",
      "steps": [
        { "type": "configure", "name": "ci-release" },
        { "type": "build", "name": "ci-release-build" }
      ]
    }
  ]
}
`
    }
  },

  "policy-block": {
    id: "policy-block",
    title: "Policy Scopes & Block",
    description: "Lexical block(SCOPE_FOR VARIABLES POLICIES PROPAGATE ...) and CMake 4.5 policy isolation.",
    defaultSymbol: "EXPORTED_TOKEN",
    files: {
      "CMakeLists.txt": `cmake_minimum_required(VERSION 3.25...4.5)
project(PolicyScopes LANGUAGES C)

set(OUTER_VAR "outer-initial")
set(EXPORTED_TOKEN "unset")

block(SCOPE_FOR VARIABLES POLICIES PROPAGATE EXPORTED_TOKEN)
  cmake_policy(SET CMP0171 NEW)
  cmake_diagnostic(SET CMD_NON_TARGET_DIRECTIVE WARN)

  set(OUTER_VAR "shadowed-inside-block")
  set(LOCAL_ONLY "ephemeral")
  set(EXPORTED_TOKEN "propagated-from-block")
endblock()

# OUTER_VAR remains "outer-initial", LOCAL_ONLY is unset,
# and EXPORTED_TOKEN holds "propagated-from-block".
add_executable(scope_check main.c)
target_compile_definitions(scope_check PRIVATE
  OUTER_VAL="\${OUTER_VAR}"
  TOKEN_VAL="\${EXPORTED_TOKEN}"
)
`
    }
  }
};

export class RcmakeWasmAdapter {
  constructor() {
    this.mode = "unloaded";
    this.module = null;
    this.bundleUrl = "./wasm/rcmake_wasm.js";
    this.engineCommit = WASM_ENGINE_COMMIT;
    this.loadReason = "WebAssembly runtime has not been loaded yet.";
    this._lastContext = null;
  }

  /**
   * Dynamically import and initialize the standalone WASI bundle.
   * Reports an error instead of falling back to precomputed output.
   *
   * @param {string} [url="./wasm/rcmake_wasm.js"]
   * @returns {Promise<{mode: "wasm"|"error", url: string, engineCommit?: string, reason?: string}>}
   */
  async load(url = "./wasm/rcmake_wasm.js") {
    this.bundleUrl = url;
    try {
      const mod = await import(url);
      if (typeof mod.default === "function") {
        await mod.default();
      }
      if (typeof mod.configure_json !== "function" || typeof mod.explain_json !== "function") {
        throw new Error("Module loaded without configure_json or explain_json exports.");
      }
      this.module = mod;
      this.mode = "wasm";
      if (typeof mod.ENGINE_COMMIT === "string" && mod.ENGINE_COMMIT.trim()) {
        this.engineCommit = mod.ENGINE_COMMIT.trim();
        this.loadReason = `Loaded WebAssembly PureHost runtime (engine ${this.engineCommit}).`;
      } else {
        this.loadReason = "Loaded WebAssembly PureHost runtime (rcmake_wasm.wasm).";
      }
      return { mode: "wasm", url, engineCommit: this.engineCommit };
    } catch (err) {
      const reason = err instanceof Error ? err.message : String(err);
      this.module = null;
      this.mode = "error";
      this.loadReason = `WebAssembly runtime failed to load: ${reason}`;
      return {
        mode: "error",
        url,
        reason
      };
    }
  }

  /**
   * Run a live configure pass over an in-memory virtual file tree.
   *
   * @param {Record<string, string>} files
   * @param {{ exampleId?: string, generator?: string, buildType?: string, preset?: string, defines?: Record<string, string> }} [options]
   */
  async configure(files, options = {}) {
    if (this.mode !== "wasm" || !this.module) {
      throw new Error(this.loadReason || "WebAssembly runtime failed to load.");
    }

    const generator = options.generator || "Ninja";
    const buildType = options.buildType || "Debug";
    const exampleId = options.exampleId || "hello-cpp20";

    const started = performance.now();
    const raw = this.module.configure_json(
      JSON.stringify({
        files,
        generator,
        buildType,
        preset: options.preset || null,
        defines: options.defines || {}
      })
    );
    const parsed = JSON.parse(raw);
    const durationMs = typeof parsed.durationMs === "number"
      ? parsed.durationMs
      : Number((performance.now() - started).toFixed(2));
    this._lastContext = {
      files,
      generator,
      buildType,
      preset: options.preset || null,
      defines: options.defines || {},
      exampleId,
      parsed
    };
    return {
      mode: "wasm",
      durationMs,
      ...parsed
    };
  }

  /**
   * Query `rcmake explain` provenance for a target or variable symbol.
   *
   * @param {string} symbol
   * @param {{ config?: string }} [options]
   */
  async explain(symbol, options = {}) {
    const trimmed = String(symbol || "").trim();
    if (!trimmed) {
      return {
        schema: "rcmake-explain-v1",
        kind: "not_found",
        name: "",
        details: { suggestions: [] }
      };
    }

    if (this.mode !== "wasm" || !this.module || typeof this.module.explain_json !== "function") {
      throw new Error(this.loadReason || "WebAssembly runtime failed to load.");
    }
    if (!this._lastContext) {
      return {
        schema: "rcmake-explain-v1",
        kind: "not_found",
        name: trimmed,
        details: { suggestions: [] }
      };
    }

    const raw = this.module.explain_json(
      JSON.stringify({
        files: this._lastContext.files,
        symbol: trimmed,
        generator: this._lastContext.generator,
        buildType: this._lastContext.buildType,
        preset: this._lastContext.preset || null,
        defines: this._lastContext.defines || {},
        config: options.config || this._lastContext.buildType
      })
    );
    return JSON.parse(raw);
  }
}
