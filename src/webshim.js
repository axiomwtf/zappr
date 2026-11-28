/* zappr web shim: minimal WASI preview1 implementation over an
 * in-memory filesystem, plus the artifact loader. Runs the zap wasm
 * host in any browser (or node, for tests). The project archive rides
 * inside the .wasm as a `zappr` custom section; the loader extracts it
 * to `/.zap-archive` before `_start`. */
(function () {
  "use strict";

  const NAME = "__ZAP_WASM_NAME__";
  const TITLE = "__ZAP_TITLE__";

  const outEl = document.getElementById("out");
  const log = (s, err) => {
    const d = document.createElement("div");
    if (err) d.style.color = "#f66";
    d.textContent = s;
    outEl.appendChild(d);
  };

  /* ---------------- WASI preview1 shim ---------------- */

  // errno values (wasi_snapshot_preview1)
  const E = {
    SUCCESS: 0, BADF: 8, EXIST: 20, INVAL: 28, IO: 29, ISDIR: 31,
    LOOP: 32, NAMETOOLONG: 37, NOENT: 44, NOSPC: 51, NOSYS: 58,
    NOTDIR: 54, NOTEMPTY: 55, NOTCAPABLE: 76, ACCES: 2,
  };

  function makeWasi(argv) {
    let inst = null;
    const dv = () => new DataView(inst.exports.memory.buffer);
    const u8 = () => new Uint8Array(inst.exports.memory.buffer);
    const enc = new TextEncoder();
    const dec = new TextDecoder();

    // in-memory FS: "/" rooted, files Map<path, {data,mtime}>, dirs Set
    const files = new Map();
    const dirs = new Set(["/"]);
    const fds = new Map();
    let nextFd = 4;

    const norm = (base, p) => {
      const raw = p.startsWith("/") ? p : base.replace(/\/$/, "") + "/" + p;
      const out = [];
      for (const c of raw.split("/")) {
        if (!c || c === ".") continue;
        if (c === "..") { out.pop(); continue; }
        out.push(c);
      }
      return "/" + out.join("/");
    };
    const fdBase = (fd) => {
      const h = fds.get(fd);
      if (!h) return null;
      return h.t === "pre" ? "/" : h.path;
    };
    const isDirPath = (p) => dirs.has(p) || p === "/";
    const fileEntry = (p) => {
      const e = files.get(p);
      if (!e) return null;
      // tolerate raw Uint8Array entries
      return e.data ? e : { data: e, mtime: 0n };
    };

    const filestatBuf = (ptr, ft, size, mtime) => {
      const d = dv();
      d.setBigUint64(ptr, 0n, true);      // dev
      d.setBigUint64(ptr + 8, 0n, true);  // ino
      d.setUint8(ptr + 16, ft);
      d.setBigUint64(ptr + 24, 1n, true); // nlink
      d.setBigUint64(ptr + 32, BigInt(size), true);
      d.setBigUint64(ptr + 40, mtime, true); // atim
      d.setBigUint64(ptr + 48, mtime, true); // mtim
      d.setBigUint64(ptr + 56, mtime, true); // ctim
    };
    const statFor = (p) => {
      if (isDirPath(p)) return { ft: 3, size: 0, mt: 0n };
      const f = fileEntry(p);
      if (f) return { ft: 4, size: f.data.length, mt: f.mtime };
      return null;
    };
    const readIovs = (ip, n) => {
      const d = dv();
      const out = [];
      for (let i = 0; i < n; i++) {
        out.push([d.getUint32(ip + i * 8, true), d.getUint32(ip + i * 8 + 4, true)]);
      }
      return out;
    };

    const wasi = {
      args_sizes_get(argvc, argvb) {
        dv().setUint32(argvc, argv.length, true);
        let n = 0;
        for (const a of argv) n += enc.encode(a).length + 1;
        dv().setUint32(argvb, n, true);
        return E.SUCCESS;
      },
      args_get(argvp, bufp) {
        const d = dv();
        let off = bufp;
        argv.forEach((a, i) => {
          const b = enc.encode(a);
          d.setUint32(argvp + i * 8, off, true);
          d.setUint32(argvp + i * 8 + 4, b.length, true);
          u8().set(b, off);
          u8()[off + b.length] = 0;
          off += b.length + 1;
        });
        return E.SUCCESS;
      },
      environ_sizes_get(ec, eb) {
        dv().setUint32(ec, 0, true);
        dv().setUint32(eb, 0, true);
        return E.SUCCESS;
      },
      environ_get() { return E.SUCCESS; },
      clock_time_get(id, _prec, retp) {
        const now =
          id === 1 ? BigInt(Math.floor(performance.now() * 1e6))
          : id >= 2 ? 0n
          : BigInt(Date.now()) * 1000000n;
        dv().setBigUint64(retp, now, true);
        return E.SUCCESS;
      },
      random_get(ptr, len) {
        crypto.getRandomValues(new Uint8Array(inst.exports.memory.buffer, ptr, len));
        return E.SUCCESS;
      },
      proc_exit(code) {
        const e = new Error("proc_exit");
        e.zapExit = code;
        throw e;
      },
      fd_close(fd) {
        if (fd <= 2 || !fds.delete(fd)) return E.BADF;
        return E.SUCCESS;
      },
      fd_fdstat_get(fd, ptr) {
        const h = fds.get(fd);
        if (!h) return E.BADF;
        const d = dv();
        const ft =
          fd <= 2 ? 2 : h.t === "dir" || h.t === "pre" ? 3 : 4;
        d.setUint8(ptr, ft);
        d.setUint16(ptr + 2, h.append ? 1 : 0, true);
        d.setBigUint64(ptr + 8, ~0n, true);
        d.setBigUint64(ptr + 16, ~0n, true);
        return E.SUCCESS;
      },
      fd_filestat_get(fd, ptr) {
        const h = fds.get(fd);
        if (!h) return E.BADF;
        const st = h.t === "file"
          ? { ft: 4, size: h.data.length, mt: h.mtime }
          : h.t === "dir" || h.t === "pre"
            ? { ft: 3, size: 0, mt: 0n }
            : { ft: 2, size: 0, mt: 0n };
        filestatBuf(ptr, st.ft, st.size, st.mt);
        return E.SUCCESS;
      },
      path_filestat_get(fd, _flags, pp, pl, bufp) {
        const base = fdBase(fd);
        if (base === null) return E.BADF;
        const p = norm(base, dec.decode(u8().subarray(pp, pp + pl)));
        const st = statFor(p);
        if (!st) return E.NOENT;
        filestatBuf(bufp, st.ft, st.size, st.mt);
        return E.SUCCESS;
      },
      fd_prestat_get(fd, ptr) {
        if (fd !== 3) return E.BADF;
        const d = dv();
        d.setUint8(ptr, 0); // dir
        d.setUint32(ptr + 4, 1, true); // name len
        return E.SUCCESS;
      },
      fd_prestat_dir_name(fd, ptr, len) {
        if (fd !== 3 || len < 1) return E.BADF;
        u8()[ptr] = 0x2f; // "/"
        return E.SUCCESS;
      },
      fd_read(fd, iovp, iovn, nreadp) {
        const h = fds.get(fd);
        if (!h) return E.BADF;
        if (h.t === "std") {
          dv().setUint32(nreadp, 0, true); // stdin: EOF
          return E.SUCCESS;
        }
        if (h.t !== "file") return E.ISDIR;
        const entry = fileEntry(h.path);
        const data = entry ? entry.data : h.data;
        let off = h.offset;
        let total = 0;
        const mem = u8();
        for (const [bp, bl] of readIovs(iovp, iovn)) {
          const avail = Math.max(0, data.length - off);
          const n = Math.min(bl, avail);
          if (n > 0) { mem.set(data.subarray(off, off + n), bp); off += n; total += n; }
          if (n < bl) break;
        }
        h.offset = off;
        dv().setUint32(nreadp, total, true);
        return E.SUCCESS;
      },
      fd_write(fd, iovp, iovn, nwrittenp) {
        const h = fds.get(fd);
        if (!h) return E.BADF;
        const mem = u8();
        const chunks = [];
        for (const [bp, bl] of readIovs(iovp, iovn)) {
          chunks.push(mem.slice(bp, bp + bl));
        }
        const len = chunks.reduce((a, c) => a + c.length, 0);
        if (h.t === "std") {
          const text = dec.decode(concatBytes(chunks));
          if (fd === 1) stdoutWrite(text);
          else stderrWrite(text);
          dv().setUint32(nwrittenp, len, true);
          return E.SUCCESS;
        }
        if (h.t !== "file") return E.BADF;
        const cur = fileEntry(h.path);
        const old = cur ? cur.data : new Uint8Array(0);
        const pos = h.append ? old.length : h.offset;
        const need = Math.max(old.length, pos + len);
        let buf = new Uint8Array(Math.max(need, old.length * 2, 64));
        buf.set(old.subarray(0, old.length));
        let o = 0;
        for (const c of chunks) { buf.set(c, pos + o); o += c.length; }
        const data = buf.slice(0, need);
        const mtime = BigInt(Date.now()) * 1000000n;
        files.set(h.path, { data, mtime });
        h.data = data;
        h.mtime = mtime;
        h.offset = pos + len;
        dv().setUint32(nwrittenp, len, true);
        return E.SUCCESS;
      },
      path_create_directory(fd, pp, pl) {
        const base = fdBase(fd);
        if (base === null) return E.BADF;
        const p = norm(base, dec.decode(u8().subarray(pp, pp + pl)));
        if (fileEntry(p)) return E.EXIST;
        dirs.add(p);
        return E.SUCCESS;
      },
      path_open(dirfd, _dirflags, pp, pl, oflags, rightsBase, _rightsInh, fdflags, outfd) {
        const base = fdBase(dirfd);
        if (base === null) return E.BADF;
        const p = norm(base, dec.decode(u8().subarray(pp, pp + pl)));
        const wantDir = (oflags & 2) !== 0;
        const creat = (oflags & 1) !== 0;
        const isDir = isDirPath(p);
        const fe = fileEntry(p);

        if (wantDir) {
          if (fe || !isDir) return E.NOTDIR;
          const fd = nextFd++;
          fds.set(fd, { t: "dir", path: p });
          dv().setUint32(outfd, fd, true);
          return E.SUCCESS;
        }
        if (isDir) return E.ISDIR;
        if (!fe && !creat) return E.NOENT;

        const fd = nextFd++;
        const canRead = (rightsBase & 2n) !== 0n;
        const canWrite = (rightsBase & 64n) !== 0n;
        const data = fe ? fe.data : new Uint8Array(0);
        const append = (fdflags & 1) !== 0;
        fds.set(fd, {
          t: "file", path: p, data,
          offset: append ? data.length : 0,
          append, write: canWrite, read: canRead,
          mtime: fe ? fe.mtime : 0n,
        });
        if (!fe) files.set(p, { data, mtime: 0n });
        dv().setUint32(outfd, fd, true);
        return E.SUCCESS;
      },
      path_unlink_file(fd, pp, pl) {
        const base = fdBase(fd);
        if (base === null) return E.BADF;
        const p = norm(base, dec.decode(u8().subarray(pp, pp + pl)));
        if (!files.delete(p)) return E.NOENT;
        return E.SUCCESS;
      },
      path_remove_directory(fd, pp, pl) {
        const base = fdBase(fd);
        if (base === null) return E.BADF;
        const p = norm(base, dec.decode(u8().subarray(pp, pp + pl)));
        for (const k of files.keys()) {
          if (k.startsWith(p === "/" ? "/" : p + "/")) return E.NOTEMPTY;
        }
        for (const k of dirs) {
          if (k !== p && k.startsWith(p === "/" ? "/" : p + "/")) return E.NOTEMPTY;
        }
        if (!dirs.delete(p)) return E.NOENT;
        return E.SUCCESS;
      },
      path_rename(ofd, opp, opl, nfd, npp, npl) {
        const ob = fdBase(ofd), nb = fdBase(nfd);
        if (ob === null || nb === null) return E.BADF;
        const op = norm(ob, dec.decode(u8().subarray(opp, opp + opl)));
        const np = norm(nb, dec.decode(u8().subarray(npp, npp + npl)));
        const fe = files.get(op);
        if (fe) { files.delete(op); files.set(np, fe); return E.SUCCESS; }
        if (dirs.has(op)) {
          dirs.delete(op); dirs.add(np);
          for (const k of [...files.keys()]) {
            if (k.startsWith(op + "/")) {
              files.set(np + k.slice(op.length), files.get(k));
              files.delete(k);
            }
          }
          for (const k of [...dirs]) {
            if (k.startsWith(op + "/")) { dirs.delete(k); dirs.add(np + k.slice(op.length)); }
          }
          return E.SUCCESS;
        }
        return E.NOENT;
      },
      path_readlink() { return E.INVAL; },
      fd_readdir(fd, bufp, buflen, cookie, bufusedp) {
        const h = fds.get(fd);
        if (!h || (h.t !== "dir" && h.t !== "pre")) return E.BADF;
        const base = h.path === undefined ? "/" : h.path;
        const names = [];
        if (base !== "/") {
          const prefix = base === "/" ? "/" : base + "/";
          for (const k of dirs) {
            if (k.startsWith(prefix)) {
              const rest = k.slice(prefix.length);
              if (rest && !rest.includes("/")) names.push(rest);
            }
          }
          for (const k of files.keys()) {
            if (k.startsWith(prefix)) {
              const rest = k.slice(prefix.length);
              if (rest && !rest.includes("/")) names.push(rest);
            }
          }
        } else {
          for (const k of dirs) {
            if (k !== "/" && !k.slice(1).includes("/")) names.push(k.slice(1));
          }
          for (const k of files.keys()) {
            if (!k.slice(1).includes("/")) names.push(k.slice(1));
          }
        }
        names.sort();
        // dedupe (a path can't be both file and dir, but be safe)
        const uniq = [...new Set(names)];
        // dirent: d_next u64, d_ino u64, d_namlen u32, d_type u8,
        // 3 pad bytes, name[] — header 24, record 8-aligned
        let off = 0;
        const recs = uniq.map((n) => {
          const nb = enc.encode(n);
          const size = (24 + nb.length + 7) & ~7;
          const rec = { name: nb, off, size, next: 0, isDir: dirs.has(norm(base, n)) && !files.has(norm(base, n)) };
          off += size;
          return rec;
        });
        for (let i = 0; i < recs.length; i++) recs[i].next = i + 1 < recs.length ? recs[i + 1].off : off;
        const startIdx = recs.findIndex((r) => r.off >= Number(cookie)) ;
        const mem = u8();
        let pos = bufp, used = 0;
        for (let i = startIdx < 0 ? recs.length : startIdx; i < recs.length; i++) {
          const r = recs[i];
          if (pos + r.size > bufp + buflen) break;
          const d = dv();
          d.setBigUint64(pos, BigInt(r.next), true);
          d.setBigUint64(pos + 8, BigInt(i + 1), true);
          d.setUint32(pos + 16, r.name.length, true);
          d.setUint8(pos + 20, r.isDir ? 4 : 8);
          mem.set(r.name, pos + 24);
          pos += r.size;
          used += r.size;
        }
        dv().setUint32(bufusedp, used, true);
        return E.SUCCESS;
      },
    };

    // stdio
    fds.set(0, { t: "std", kind: "in" });
    fds.set(1, { t: "std", kind: "out" });
    fds.set(2, { t: "std", kind: "err" });
    fds.set(3, { t: "pre", path: "/" });

    return {
      wasi,
      bind(instance) { inst = instance; },
      files, dirs,
    };
  }

  function concatBytes(chunks) {
    const n = chunks.reduce((a, c) => a + c.length, 0);
    const out = new Uint8Array(n);
    let o = 0;
    for (const c of chunks) { out.set(c, o); o += c.length; }
    return out;
  }

  // stdout line buffering -> console
  let outBuf = "", errBuf = "";
  function stdoutWrite(t) {
    outBuf += t;
    let i;
    while ((i = outBuf.indexOf("\n")) >= 0) {
      log(outBuf.slice(0, i).replace(/\n$/, ""));
      outBuf = outBuf.slice(i + 1);
    }
  }
  function stderrWrite(t) {
    errBuf += t;
    let i;
    while ((i = errBuf.indexOf("\n")) >= 0) {
      log(errBuf.slice(0, i), true);
      errBuf = errBuf.slice(i + 1);
    }
  }
  function flush() {
    if (outBuf) { log(outBuf); outBuf = ""; }
    if (errBuf) { log(errBuf, true); errBuf = ""; }
  }

  /* ---------------- wasm section helpers ---------------- */

  function readU32leb(b, p) {
    let r = 0, s = 0;
    for (;;) {
      const x = b[p++];
      r |= (x & 0x7f) << s;
      if (!(x & 0x80)) break;
      s += 7;
    }
    return [r >>> 0, p];
  }
  function u32leb(n) {
    const out = [];
    do { let x = n & 0x7f; n >>>= 7; if (n) x |= 0x80; out.push(x); } while (n);
    return out;
  }
  function writeU32leb(n) {
    const b = new Uint8Array(u32leb(n));
    return b;
  }

  // find the `zappr` custom section payload (the packed project archive)
  function extractProject(wasmBytes) {
    let p = 8; // skip magic + version
    while (p < wasmBytes.length) {
      const id = wasmBytes[p++];
      let r = readU32leb(wasmBytes, p);
      const size = r[0]; p = r[1];
      const end = p + size;
      if (id === 0) {
        r = readU32leb(wasmBytes, p);
        const nl = r[0]; p = r[1];
        const nm = new TextDecoder().decode(wasmBytes.subarray(p, p + nl));
        p += nl;
        if (nm === "zappr") {
          return wasmBytes.slice(p, end);
        }
      }
      p = end;
    }
    return null;
  }

  /* ---------------- web GUI (canvas backend) ----------------
   * The wasm host can't block on a native loop, so ui.run attaches
   * the marshalled tree and returns; from here JS owns the loop:
   * pointer events are fed in, zap_gui_step renders a frame per
   * animation frame, and damage rects go straight to putImageData. */

  function startGui(ex) {
    const packed = ex.zap_gui_size();
    const w = Number(packed & 0xffffffffn);
    const h = Number(packed >> 32n);
    const cv = document.createElement("canvas");
    cv.width = w;
    cv.height = h;
    cv.style.cssText =
      "display:block;margin:1em auto;image-rendering:pixelated;" +
      "max-width:96vw;max-height:80vh;background:#000;cursor:default";
    document.body.appendChild(cv);
    const ctx = cv.getContext("2d");
    const img = ctx.createImageData(w, h);
    const px = img.data;
    const toFb = (e) => {
      const r = cv.getBoundingClientRect();
      return [
        Math.max(0, Math.min(w - 1, Math.floor(((e.clientX - r.left) * w) / r.width))),
        Math.max(0, Math.min(h - 1, Math.floor(((e.clientY - r.top) * h) / r.height))),
      ];
    };
    cv.addEventListener("mousemove", (e) => {
      const p = toFb(e); ex.zap_gui_feed(p[0], p[1], 0);
    });
    cv.addEventListener("mousedown", (e) => {
      const p = toFb(e); ex.zap_gui_feed(p[0], p[1], 1);
      e.preventDefault();
    });
    window.addEventListener("mouseup", (e) => {
      const p = toFb(e); ex.zap_gui_feed(p[0], p[1], 2);
    });
    const pump = () => {
      const n = ex.zap_gui_step();
      if (n > 0) {
        const fb = ex.zap_gui_fb();
        const u32 = new Uint32Array(ex.memory.buffer, fb, w * h);
        for (let i = 0, o = 0; i < u32.length; i++, o += 4) {
          const v = u32[i];
          px[o] = (v >> 16) & 255;     // R
          px[o + 1] = (v >> 8) & 255;  // G
          px[o + 2] = v & 255;         // B
          px[o + 3] = 255;             // A
        }
        const dp = ex.zap_gui_damage_ptr();
        const du = new Uint32Array(ex.memory.buffer, dp, n * 4);
        for (let i = 0; i < n; i++) {
          const o = i * 4;
          ctx.putImageData(img, 0, 0, du[o], du[o + 1], du[o + 2], du[o + 3]);
        }
      }
      requestAnimationFrame(pump);
    };
    requestAnimationFrame(pump);
    log("gui mode: " + w + "x" + h + " canvas");
  }

  /* ---------------- boot ---------------- */

  (async () => {
    try {
      log("loading " + NAME + " …");
      let buf = await (await fetch(NAME)).arrayBuffer();
      let bytes = new Uint8Array(buf);
      // strip ZAPR signature trailer (4 magic + 32 hash + 64 sig)
      const TRAILER = 100;
      if (bytes.length > TRAILER &&
          bytes[bytes.length - TRAILER] === 0x5a &&
          bytes[bytes.length - TRAILER + 1] === 0x41 &&
          bytes[bytes.length - TRAILER + 2] === 0x50 &&
          bytes[bytes.length - TRAILER + 3] === 0x52) {
        bytes = bytes.slice(0, bytes.length - TRAILER);
      }
      const archive = extractProject(bytes);

      // argv: argv0 + one arg per ?key=value query pair
      const argv = [TITLE];
      new URLSearchParams(location.search).forEach((v, k) => argv.push(k + "=" + v));

      const shim = makeWasi(argv);
      const imports = { wasi_snapshot_preview1: shim.wasi };
      const mod = await WebAssembly.compile(bytes);
      const instance = await WebAssembly.instantiate(mod, imports);
      shim.bind(instance);
      if (archive) {
        shim.files.set("/.zap-archive", { data: archive, mtime: 0n });
        log("project archive loaded (" + archive.length + " bytes)");
      }
      try {
        instance.exports._start();
      } catch (e) {
        if (e && e.zapExit !== undefined) {
          log("[exit " + e.zapExit + "]");
        } else {
          throw e;
        }
      }
      const ex = instance.exports;
      if (typeof ex.zap_gui_active === "function" && ex.zap_gui_active() === 1) {
        startGui(ex);
      }
      flush();
    } catch (e) {
      log("wasm error: " + (e && e.stack ? e.stack : e), true);
      flush();
    }
  })();
})();
