// SPDX-License-Identifier: MIT OR Apache-2.0

const encoder = new TextEncoder();
const decoder = new TextDecoder();
let compiledModule;
let compilation;

async function compile(url) {
  if (url.protocol === "file:" && globalThis.process?.versions?.node) {
    const { readFile } = await import("node:fs/promises");
    return WebAssembly.compile(await readFile(url));
  }
  const response = await fetch(url);
  if (!response.ok) {
    throw new Error(`Could not load the rcmake runtime (HTTP ${response.status}).`);
  }
  if (typeof WebAssembly.compileStreaming === "function") {
    try {
      return await WebAssembly.compileStreaming(response.clone());
    } catch {
      // Some static hosts serve WebAssembly without the WebAssembly media type.
    }
  }
  return WebAssembly.compile(await response.arrayBuffer());
}

export default async function init(wasmUrl = "./rcmake_wasm.wasm") {
  compilation ??= compile(new URL(wasmUrl, import.meta.url)).catch((error) => {
    compilation = undefined;
    throw error;
  });
  compiledModule = await compilation;
}

export function configure_json(requestJson) {
  return run("configure", requestJson);
}

export function explain_json(requestJson) {
  return run("explain", requestJson);
}

const SUCCESS = 0;
const BADF = 8;
const INVAL = 28;
const NOTSUP = 58;
const AGAIN = 6;
const RIGHTS_READ = 1n << 1n;
const RIGHTS_WRITE = 1n << 6n;
const RIGHTS_POLL = 1n << 27n;

// The command receives stdin, stdout, and stderr. It uses PureHost for files,
// the environment, and process operations.
function run(op, requestJson) {
  if (!compiledModule) {
    throw new Error("Initialize the rcmake runtime before sending a request.");
  }
  const input = encoder.encode(JSON.stringify({ ...JSON.parse(requestJson), op }));
  const outputs = { 1: [], 2: [] };
  const open = new Set([0, 1, 2]);
  const exit = new Error("WASI command exited.");
  let offset = 0;
  let exitCode = 0;
  let instance;

  const memory = () => instance.exports.memory.buffer;
  const view = () => new DataView(memory());
  const bytes = (address, size) => new Uint8Array(memory(), address, size);
  const writeU32 = (address, value) => view().setUint32(address, value, true);
  const writeU64 = (address, value) => view().setBigUint64(address, value, true);
  const clock = (id) => {
    if (id === 0) return BigInt(Date.now()) * 1_000_000n;
    if (id === 1) return BigInt(Math.trunc(performance.now() * 1_000_000));
    return null;
  };
  const iovecs = function* (ptr, count) {
    const data = view();
    for (let index = 0; index < count; index += 1) {
      const entry = ptr + index * 8;
      yield bytes(data.getUint32(entry, true), data.getUint32(entry + 4, true));
    }
  };
  const wasi = {
    fd_read(fd, ptr, count, result) {
      if (fd !== 0 || !open.has(fd)) return BADF;
      let total = 0;
      for (const buffer of iovecs(ptr, count)) {
        const chunk = input.subarray(offset, offset + buffer.length);
        buffer.set(chunk);
        offset += chunk.length;
        total += chunk.length;
        if (chunk.length < buffer.length) break;
      }
      writeU32(result, total);
      return SUCCESS;
    },
    fd_write(fd, ptr, count, result) {
      if (!(fd in outputs) || !open.has(fd)) return BADF;
      let total = 0;
      for (const buffer of iovecs(ptr, count)) {
        outputs[fd].push(buffer.slice());
        total += buffer.length;
      }
      writeU32(result, total);
      return SUCCESS;
    },
    fd_close(fd) {
      return open.delete(fd) ? SUCCESS : BADF;
    },
    fd_fdstat_get(fd, result) {
      if (!open.has(fd)) return BADF;
      bytes(result, 24).fill(0);
      bytes(result, 1)[0] = 2; // Character device.
      writeU64(result + 8, RIGHTS_POLL | (fd === 0 ? RIGHTS_READ : RIGHTS_WRITE));
      return SUCCESS;
    },
    fd_prestat_get: () => BADF,
    fd_prestat_dir_name: () => BADF,
    environ_sizes_get(count, size) {
      writeU32(count, 0);
      writeU32(size, 0);
      return SUCCESS;
    },
    environ_get: () => SUCCESS,
    args_sizes_get(count, size) {
      writeU32(count, 0);
      writeU32(size, 0);
      return SUCCESS;
    },
    args_get: () => SUCCESS,
    clock_time_get(id, _precision, result) {
      const now = clock(id);
      if (now === null) return INVAL;
      writeU64(result, now);
      return SUCCESS;
    },
    random_get(ptr, size) {
      for (let index = 0; index < size; index += 65536) {
        globalThis.crypto.getRandomValues(bytes(ptr + index, Math.min(65536, size - index)));
      }
      return SUCCESS;
    },
    proc_exit(code) {
      exitCode = code;
      throw exit;
    },
    poll_oneoff(subscriptions, events, count, written) {
      if (count === 0) return INVAL;
      let ready = 0;
      const data = view();
      for (let index = 0; index < count; index += 1) {
        const entry = subscriptions + index * 48;
        const type = data.getUint8(entry + 8);
        const id = data.getUint32(entry + 16, true);
        let error = SUCCESS;
        let available = 0;
        if (type === 0) {
          const timeout = data.getBigUint64(entry + 24, true);
          const absolute = (data.getUint16(entry + 40, true) & 1) !== 0;
          const now = clock(id);
          if (now === null) error = INVAL;
          else if (timeout > (absolute ? now : 0n)) continue;
        } else if (type === 1 && id === 0 && open.has(id)) {
          available = input.length - offset;
        } else if (type !== 2 || !(id in outputs) || !open.has(id)) {
          error = type > 2 ? INVAL : BADF;
        }
        const event = events + ready * 32;
        bytes(event, 32).fill(0);
        writeU64(event, data.getBigUint64(entry, true));
        data.setUint16(event + 8, error, true);
        data.setUint8(event + 10, type);
        writeU64(event + 16, BigInt(available));
        if (type === 1 && error === SUCCESS && available === 0) {
          data.setUint16(event + 24, 1, true); // End of input.
        }
        ready += 1;
      }
      writeU32(written, ready);
      // The synchronous exports cannot wait for future events.
      return ready === 0 ? AGAIN : SUCCESS;
    },
    sched_yield: () => SUCCESS,
    fd_seek: () => NOTSUP,
  };

  instance = new WebAssembly.Instance(compiledModule, { wasi_snapshot_preview1: wasi });
  try {
    instance.exports._start();
  } catch (error) {
    if (error !== exit) throw error;
  }
  if (exitCode !== 0) {
    const stderr = decode(outputs[2]).trim();
    throw new Error(stderr || `The rcmake runtime exited with code ${exitCode}.`);
  }
  return decode(outputs[1]);
}

function decode(chunks) {
  return chunks.map((chunk) => decoder.decode(chunk, { stream: true })).join("") + decoder.decode();
}
