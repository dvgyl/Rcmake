// SPDX-License-Identifier: MIT OR Apache-2.0

import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { readFile } from "node:fs/promises";
import { pathToFileURL, fileURLToPath } from "node:url";
import { PLAYGROUND_EXAMPLES } from "../../../site/wasm-adapter.js";
import init, * as runtime from "../../../site/wasm/rcmake_wasm.js";

const site = new URL("../../../site/", import.meta.url);
const wasm = new URL("wasm/rcmake_wasm.wasm", site);

const expected = {
  "hello-cpp20": ["hello", "HELLO_TRACE=1", "target"],
  "library-export": ["demo_cli", "GEOM_API_VERSION=2", "target"],
  "genex": ["codec_app", "CODEC_ASSERTIONS=1", "target"],
  "presets-v13": ["bench", "build-Release.ninja", "variable"],
  "policy-block": ["scope_check", "propagated-from-block", "variable"],
};
const cases = Object.entries(PLAYGROUND_EXAMPLES).map(([id, example]) => ({
  id,
  request: {
    files: example.files,
    generator: id === "presets-v13" ? "Ninja Multi-Config" : "Ninja",
    buildType: "Debug",
  },
  symbol: example.defaultSymbol,
  expected: expected[id],
}));
cases.push({
  id: "custom-project",
  request: {
    files: {
      "CMakeLists.txt": [
        "cmake_minimum_required(VERSION 4.5)",
        "project(Custom LANGUAGES CXX)",
        "add_library(flags INTERFACE)",
        "target_compile_definitions(flags INTERFACE FOO=42)",
        "add_subdirectory(lib)",
      ].join("\n"),
      "lib/CMakeLists.txt": [
        "set(SOURCE foo.cpp)",
        "add_executable(my_custom_tool ${SOURCE})",
        "target_link_libraries(my_custom_tool PRIVATE flags)",
        "target_compile_definitions(my_custom_tool PRIVATE BAR=7)",
      ].join("\n"),
    },
    generator: "Ninja",
    buildType: "Debug",
  },
  symbol: "my_custom_tool",
  expected: ["my_custom_tool", "FOO=42", "target"],
});
cases.push({
  id: "syntax-error",
  request: {
    files: { "CMakeLists.txt": "cmake_minimum_required(VERSION 4.5)\nadd_executable(ç broken.cpp\n" },
    generator: "Ninja",
    buildType: "Debug",
  },
});

async function exercise({ configure_json, explain_json }, cases) {
  const check = (condition, message) => {
    if (!condition) throw new Error(message);
  };
  const passed = [];
  for (const { id, request, symbol, expected } of cases) {
    const output = JSON.parse(await configure_json(JSON.stringify(request)));
    passed.push(id);
    if (!expected) {
      check(!output.ok && output.diagnostics.some((diag) =>
        diag.severity === "error" && diag.file === "CMakeLists.txt" &&
        diag.line >= 2 && diag.message.includes("Parse error")), JSON.stringify(output));
      continue;
    }
    const [target, needle, kind] = expected;
    check(output.ok, `${id}: ${JSON.stringify(output.diagnostics)}`);
    const targetInfo = output.targets.find((entry) => entry.name === target);
    check(targetInfo, `${id}: Target ${target} is missing.`);
    check(output.files["build.ninja"], `${id}: build.ninja is missing.`);
    check(Object.entries(output.files).flat().some((entry) => entry.includes(needle)), `${id}: Expected ${needle} in the build files.`);
    const report = JSON.parse(await explain_json(JSON.stringify({ ...request, symbol, config: "Debug" })));
    check(report.schema === "rcmake-explain-v1" && report.kind === kind, `${id}: ${JSON.stringify(report)}`);
    if (id === "custom-project") {
      check(["FOO=42", "BAR=7"].every((value) => targetInfo.compileDefinitions.includes(value)), JSON.stringify(targetInfo));
      const provenance = JSON.stringify(report);
      check(provenance.includes("FOO=42") && provenance.includes('"chain":[{'), provenance);
      check(output.files["build.ninja"].includes("/src/lib/foo.cpp"), output.files["build.ninja"]);
    }
  }
  return passed;
}

await init();
console.log(`Node.js ESM passed ${(await exercise(runtime, cases)).join(", ")}.`);

if (process.argv.includes("--wasmtime")) {
  const invoke = (op, request) => {
    const result = spawnSync("wasmtime", ["run", fileURLToPath(wasm)], {
      input: JSON.stringify({ ...JSON.parse(request), op }),
      encoding: "utf8",
    });
    if (result.error) throw result.error;
    assert.equal(result.status, 0, result.stderr);
    return result.stdout;
  };
  const passed = await exercise({
    configure_json: (request) => invoke("configure", request),
    explain_json: (request) => invoke("explain", request),
  }, cases);
  console.log(`Wasmtime CLI passed ${passed.join(", ")}.`);
}

if (process.argv.includes("--browser")) {
  const { chromium } = process.env.PLAYWRIGHT_MODULE
    ? await import(pathToFileURL(process.env.PLAYWRIGHT_MODULE))
    : await import("playwright");
  const browser = await chromium.launch({ headless: true });
  try {
    const page = await browser.newPage();
    const errors = [];
    page.on("pageerror", (error) => errors.push(error.message));
    page.on("console", (message) => {
      if (message.type() === "error") errors.push(message.text());
    });
    const adapterSource = await readFile(new URL("wasm-adapter.js", site), "utf8");
    const runtimeSource = await readFile(new URL("wasm/rcmake_wasm.js", site), "utf8");
    const wasmBytes = await readFile(wasm);
    const asyncRuntimeSource = runtimeSource.includes("async function run(")
      ? runtimeSource
      : runtimeSource.replace("function run(", "async function run(")
        .replace("new WebAssembly.Instance(", "await WebAssembly.instantiate(");
    await page.route("http://rcmake.test/**", async (route) => {
      const pathname = new URL(route.request().url()).pathname;
      const asset = {
        "/": { body: "<!doctype html><title>Rcmake WASM test</title>", contentType: "text/html" },
        "/wasm-adapter.js": { body: adapterSource, contentType: "text/javascript" },
        "/wasm/rcmake_wasm.js": { body: runtimeSource, contentType: "text/javascript" },
        "/wasm/async.js": { body: asyncRuntimeSource, contentType: "text/javascript" },
        "/wasm/rcmake_wasm.wasm": { body: wasmBytes, contentType: "application/wasm" },
        "/fallback.wasm": { body: wasmBytes, contentType: "application/octet-stream" },
        "/exercise.js": { body: `export ${exercise.toString()}`, contentType: "text/javascript" },
      }[pathname];
      await route.fulfill(asset ?? { status: 404, body: "" });
    });
    await page.goto("http://rcmake.test/");
    const passed = await page.evaluate(async (cases) => {
      const { exercise } = await import("/exercise.js");
      const { RcmakeWasmAdapter } = await import("/wasm-adapter.js");
      const adapter = new RcmakeWasmAdapter();
      const state = await adapter.load();
      if (state.mode !== "wasm") throw new Error(state.reason);
      const custom = cases.find(({ id }) => id === "custom-project");
      const configured = await adapter.configure(custom.request.files, custom.request);
      if (!configured.ok || configured.mode !== "wasm") throw new Error(JSON.stringify(configured));
      const report = await adapter.explain(custom.symbol);
      if (report.kind !== "target") throw new Error(JSON.stringify(report));
      const fallback = await import("/wasm/rcmake_wasm.js?fallback");
      await fallback.default("/fallback.wasm");
      const asyncRuntime = await import("/wasm/async.js");
      await asyncRuntime.default();
      return {
        streaming: await exercise(adapter.module, cases),
        fallback: await exercise(fallback, [custom]),
        async: await exercise(asyncRuntime, [custom]),
      };
    }, cases);
    assert.deepEqual(errors, []);
    console.log(`Chromium ${browser.version()} passed asynchronous WASM exports through the adapter with zero console or page errors.`);
    console.log(`Chromium cases passed ${passed.streaming.join(", ")}.`);
    console.log(`Chromium alternate media type passed ${passed.fallback.join(", ")}.`);
    console.log(`Chromium asynchronous instantiation passed ${passed.async.join(", ")}.`);
  } finally {
    await browser.close();
  }
}
