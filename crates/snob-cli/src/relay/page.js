// The page shown in the person's own browser: it draws the frames snob's
// browser sends and posts back what the person does with the mouse, the
// keyboard and the touch screen. See `relay/mod.rs` and `relay/input.rs`.
"use strict";

(() => {
  const screen = document.getElementById("screen");
  const keys = document.getElementById("keys");
  const address = document.getElementById("address");
  const done = document.getElementById("done");

  // The tab's size in its own CSS pixels, which the coordinates are sent in.
  let size = { w: 1, h: 1 };

  // Input is posted in order, a batch at a time. A move with no button held
  // replaces the one before it that has not gone yet.
  const queue = [];
  let moved = null;
  let sending = false;

  function send(event) {
    if (event.t === "mouse" && event.kind === "move" && !event.buttons) {
      moved = event;
    } else {
      if (moved) {
        queue.push(moved);
        moved = null;
      }
      queue.push(event);
    }
    if (!sending) {
      flush();
    }
  }

  async function flush() {
    sending = true;
    while (queue.length || moved) {
      if (moved) {
        queue.push(moved);
        moved = null;
      }
      const batch = queue.splice(0, 64);
      try {
        await fetch("/input", {
          method: "POST",
          headers: { "Content-Type": "application/json", "X-Snob-Input": "1" },
          body: JSON.stringify(batch),
          credentials: "same-origin",
        });
      } catch (_) {
        queue.length = 0;
        break;
      }
    }
    sending = false;
  }

  const modifiers = (e) =>
    (e.altKey ? 1 : 0) | (e.ctrlKey ? 2 : 0) | (e.metaKey ? 4 : 0) | (e.shiftKey ? 8 : 0);

  // Windows reports AltGr as Ctrl and Alt held together, so the "@" of AltGr+2
  // on a Spanish keyboard would arrive as a shortcut and type nothing. A key
  // that AltGr turned into a character keeps neither: only Shift is left. A
  // letter or a digit with Ctrl and Alt is a real shortcut and stays one.
  const altGraph = (e) =>
    (e.getModifierState && e.getModifierState("AltGraph")) ||
    (e.ctrlKey && e.altKey && [...e.key].length === 1 && !/^[a-z0-9]$/i.test(e.key));
  const keyModifiers = (e) => (altGraph(e) ? modifiers(e) & 8 : modifiers(e));

  function at(e) {
    const r = screen.getBoundingClientRect();
    const x = ((e.clientX - r.left) * size.w) / Math.max(r.width, 1);
    const y = ((e.clientY - r.top) * size.h) / Math.max(r.height, 1);
    return {
      x: Math.max(0, Math.min(size.w, x)),
      y: Math.max(0, Math.min(size.h, y)),
    };
  }

  const buttons = ["left", "middle", "right"];

  // Pointer events carry no click count, so it is counted here: presses
  // close together in time and place are one double or triple click.
  let last = { time: 0, x: 0, y: 0, count: 0 };
  function clicks(e) {
    const now = performance.now();
    const near = Math.abs(e.clientX - last.x) < 5 && Math.abs(e.clientY - last.y) < 5;
    const count = now - last.time < 500 && near ? Math.min(last.count + 1, 3) : 1;
    last = { time: now, x: e.clientX, y: e.clientY, count };
    return count;
  }

  // A finger: a tap is a click, a drag scrolls.
  let touch = null;

  screen.addEventListener("pointerdown", (e) => {
    e.preventDefault();
    keys.focus({ preventScroll: true });
    if (e.pointerType === "touch") {
      touch = { id: e.pointerId, x: e.clientX, y: e.clientY, at: at(e), scrolled: false };
      return;
    }
    screen.setPointerCapture(e.pointerId);
    const p = at(e);
    send({
      t: "mouse",
      kind: "down",
      x: p.x,
      y: p.y,
      button: buttons[e.button] || "left",
      buttons: e.buttons,
      clicks: clicks(e),
      mods: modifiers(e),
    });
  });

  screen.addEventListener("pointermove", (e) => {
    if (e.pointerType === "touch") {
      if (!touch || touch.id !== e.pointerId) {
        return;
      }
      const dx = touch.x - e.clientX;
      const dy = touch.y - e.clientY;
      if (!touch.scrolled && Math.hypot(dx, dy) < 8) {
        return;
      }
      touch.scrolled = true;
      const r = screen.getBoundingClientRect();
      const scale = size.w / Math.max(r.width, 1);
      send({ t: "wheel", x: touch.at.x, y: touch.at.y, dx: dx * scale, dy: dy * scale });
      touch.x = e.clientX;
      touch.y = e.clientY;
      return;
    }
    const p = at(e);
    send({ t: "mouse", kind: "move", x: p.x, y: p.y, buttons: e.buttons, mods: modifiers(e) });
  });

  screen.addEventListener("pointerup", (e) => {
    if (e.pointerType === "touch") {
      if (touch && touch.id === e.pointerId && !touch.scrolled) {
        const p = touch.at;
        send({ t: "mouse", kind: "move", x: p.x, y: p.y, buttons: 1 });
        send({ t: "mouse", kind: "down", x: p.x, y: p.y, button: "left", buttons: 1, clicks: 1 });
        send({ t: "mouse", kind: "up", x: p.x, y: p.y, button: "left", buttons: 0, clicks: 1 });
      }
      touch = null;
      return;
    }
    const p = at(e);
    send({
      t: "mouse",
      kind: "up",
      x: p.x,
      y: p.y,
      button: buttons[e.button] || "left",
      buttons: e.buttons,
      clicks: last.count,
      mods: modifiers(e),
    });
  });

  screen.addEventListener("pointercancel", () => {
    touch = null;
  });

  screen.addEventListener("contextmenu", (e) => e.preventDefault());

  screen.addEventListener(
    "wheel",
    (e) => {
      e.preventDefault();
      const unit = e.deltaMode === 1 ? 40 : e.deltaMode === 2 ? size.h : 1;
      const p = at(e);
      send({ t: "wheel", x: p.x, y: p.y, dx: e.deltaX * unit, dy: e.deltaY * unit, mods: modifiers(e) });
    },
    { passive: false },
  );

  // Keys reach the hidden text area, which keeps the focus: whatever it
  // would type goes to snob's browser instead.
  const composing = (e) => e.isComposing || e.keyCode === 229;
  const pasting = (e) => (e.ctrlKey || e.metaKey) && (e.key === "v" || e.key === "V");

  keys.addEventListener("keydown", (e) => {
    if (composing(e) || pasting(e)) {
      return;
    }
    e.preventDefault();
    send({
      t: "key",
      down: true,
      key: e.key,
      code: e.code,
      keyCode: e.keyCode,
      text: [...e.key].length === 1 ? e.key : "",
      mods: keyModifiers(e),
      repeat: e.repeat,
    });
  });

  keys.addEventListener("keyup", (e) => {
    if (composing(e)) {
      return;
    }
    e.preventDefault();
    send({ t: "key", down: false, key: e.key, code: e.code, keyCode: e.keyCode, mods: keyModifiers(e) });
  });

  // A paste arrives whole, as text: the clipboard of the person's machine
  // never has to be read by anything else.
  keys.addEventListener("paste", (e) => {
    e.preventDefault();
    const text = e.clipboardData ? e.clipboardData.getData("text/plain") : "";
    if (text) {
      send({ t: "text", text });
    }
  });

  // What an on-screen keyboard or an input method typed.
  keys.addEventListener("compositionend", (e) => {
    if (e.data) {
      send({ t: "text", text: e.data });
    }
    keys.value = "";
  });

  keys.addEventListener("input", (e) => {
    if (e.isComposing) {
      return;
    }
    if (e.inputType === "deleteContentBackward") {
      const backspace = { t: "key", key: "Backspace", code: "Backspace", keyCode: 8 };
      send({ ...backspace, down: true });
      send({ ...backspace, down: false });
    } else if (e.data) {
      send({ t: "text", text: e.data });
    }
    keys.value = "";
  });

  document.getElementById("back").addEventListener("click", () => send({ t: "back" }));
  document.getElementById("forward").addEventListener("click", () => send({ t: "forward" }));
  document.getElementById("reload").addEventListener("click", () => send({ t: "reload" }));
  document.getElementById("keyboard").addEventListener("click", () => keys.focus());

  const events = new EventSource("/events");
  events.addEventListener("frame", (e) => {
    const frame = JSON.parse(e.data);
    if (frame.w && frame.h) {
      size = { w: frame.w, h: frame.h };
    }
    screen.src = "data:image/jpeg;base64," + frame.data;
  });
  events.addEventListener("address", (e) => {
    address.textContent = JSON.parse(e.data);
  });
  events.addEventListener("done", () => {
    events.close();
    done.hidden = false;
  });
  events.addEventListener("error", () => {
    if (done.hidden) {
      address.textContent = "lost the connection to snob; trying again...";
    }
  });
})();
