// Tauri v2 only injects `window.__TAURI__` when the npm global API script was
// generated (it isn't for this dependency-free project). The low-level
// `window.__TAURI_INTERNALS__` bridge is always present, so talk to it directly.
const internals = window.__TAURI_INTERNALS__;
const inTauri = typeof internals?.invoke === "function";

// Raw ANC strengths (1..4) are the source of truth; the quirky ear-web UI
// level translation lives in the Rust protocol module and has its own test.

const el = (id) => document.getElementById(id);
const ui = {
  deviceName: el("device-name"),
  statusDot: el("status-dot"),
  disconnected: el("disconnected"),
  connected: el("connected"),
  connectButton: el("connect-button"),
  status: el("connection-status"),
  statusMessage: el("status-message"),
  statusLabel: el("status-label"),
  port: el("device-port"),
  firmware: el("device-firmware"),
  strengthRow: el("strength-row"),
  inEar: el("in-ear"),
};

let state = null;

let toastTimer = null;
function showError(message) {
  console.error("[cmf-tray]", message);
  const toast = el("toast");
  if (!toast) return;
  toast.textContent = message;
  toast.hidden = false;
  clearTimeout(toastTimer);
  toastTimer = setTimeout(() => {
    toast.hidden = true;
  }, 4000);
}

function invoke(name, args = {}) {
  if (inTauri) {
    return internals.invoke(name, args).catch((error) => {
      const detail = typeof error === "string" ? error : JSON.stringify(error);
      showError(`${name} failed: ${detail}`);
    });
  }
  // Browser preview fallback: keep the UI explorable without Tauri.
  if (name === "connect") state.connected = true;
  if (name === "disconnect") state.connected = false;
  if (name === "set_anc") {
    state.anc_mode = args.mode;
    if (args.strength) state.anc_strength = args.strength;
  }
  if (name === "set_eq") state.eq_preset = args.preset;
  if (name === "set_in_ear") state.in_ear_detection = args.enabled;
  render(state);
  return Promise.resolve();
}

const appSheet = () => document.querySelector(".app");

// Animate the glass sheet out of the window instead of dragging the whole
// native window over the taskbar. The window clips it at the taskbar top.
function closePopup() {
  const sheet = appSheet();
  if (!sheet || sheet.classList.contains("is-closing")) return;
  sheet.classList.add("is-closing");
  setTimeout(() => {
    invoke("hide_popup").finally(() => sheet.classList.remove("is-closing"));
  }, 190);
}

// Called when Rust shows the preloaded window, so a previous close animation
// cannot make the next open slide in from the bottom.
function resetPopup() {
  const sheet = appSheet();
  if (!sheet) return;
  sheet.classList.add("no-transition");
  sheet.classList.remove("is-closing");
  void sheet.offsetWidth;
  sheet.classList.remove("no-transition");
}

/// Subscribe to a Rust event without the npm API package.
async function listen(event, handler) {
  if (!inTauri) return () => {};
  const callbackId = internals.transformCallback((data) => handler(data), false);
  const eventId = await internals.invoke("plugin:event|listen", {
    event,
    target: { kind: "Any" },
    handler: callbackId,
  });
  return async () => {
    window.__TAURI_EVENT_PLUGIN_INTERNALS__?.unregisterListener?.(event, eventId);
    await internals.invoke("plugin:event|unlisten", { event, eventId });
  };
}

function battery(value, barId, valueId, chargeId) {
  const present = !!value?.present;
  const level = Math.max(0, Math.min(100, value.level));
  const ring = el(barId);
  // The circular progress ring reads these CSS variables.
  ring.style.setProperty("--level", present ? level : 0);
  ring.style.setProperty(
    "--ring-color",
    level <= 20 ? "#ff9f0a" : "var(--accent)",
  );
  el(valueId).textContent = present ? `${value.level}%` : "--";
  el(chargeId).hidden = !present || !value.charging;
}

function render(next) {
  if (!next) return;
  state = next;

  ui.deviceName.textContent = next.device_name || "CMF BUDS PRO 2";
  ui.statusDot.classList.toggle("is-on", next.connected);
  ui.disconnected.hidden = next.connected;
  ui.connected.hidden = !next.connected;
  ui.connectButton.textContent = next.connected ? "Disconnect" : "Connect";
  if (ui.statusLabel) {
    ui.statusLabel.textContent = next.connected ? "Connected" : "Not connected";
  }
  const statusText = next.status || (next.connected ? "Connected" : "Disconnected");
  ui.status.textContent = statusText;
  if (ui.statusMessage) ui.statusMessage.textContent = statusText;
  ui.port.textContent = next.port || "";
  ui.firmware.textContent = next.firmware ? `FW ${next.firmware}` : "";

  battery(next.left, "bar-left", "battery-left", "charge-left");
  battery(next.right, "bar-right", "battery-right", "charge-right");
  battery(next.case_battery, "bar-case", "battery-case", "charge-case");

  document.querySelectorAll("[data-anc]").forEach((button) => {
    button.classList.toggle("is-active", button.dataset.anc === next.anc_mode);
  });

  ui.strengthRow.hidden = next.anc_mode !== "anc";
  document.querySelectorAll("[data-strength]").forEach((button) => {
    button.classList.toggle(
      "is-active",
      Number(button.dataset.strength) === next.anc_strength,
    );
  });

  document.querySelectorAll("[data-eq]").forEach((button) => {
    button.classList.toggle("is-active", Number(button.dataset.eq) === next.eq_preset);
  });

  ui.inEar.checked = next.in_ear_detection;
}

function wire() {
  el("close").addEventListener("click", closePopup);
  ui.connectButton.addEventListener("click", () => invoke("connect"));
  el("disconnect-button")?.addEventListener("click", () => invoke("disconnect"));

  document.querySelectorAll("[data-anc]").forEach((button) => {
    button.addEventListener("click", () => {
      const mode = button.dataset.anc;
      invoke("set_anc", {
        mode,
        strength: mode === "anc" ? state.anc_strength : null,
      });
    });
  });

  document.querySelectorAll("[data-strength]").forEach((button) => {
    button.addEventListener("click", () =>
      invoke("set_anc", { mode: "anc", strength: Number(button.dataset.strength) }),
    );
  });

  document.querySelectorAll("[data-eq]").forEach((button) => {
    button.addEventListener("click", () =>
      invoke("set_eq", { preset: Number(button.dataset.eq) }),
    );
  });

  ui.inEar.addEventListener("change", () => {
    const requested = ui.inEar.checked;
    // Never let the browser pretend the buds changed: revert immediately and
    // wait for the decoded response from Rust to update the switch.
    ui.inEar.checked = state?.in_ear_detection ?? false;
    invoke("set_in_ear", { enabled: requested });
  });

  document.addEventListener("keydown", (event) => {
    if (event.key === "Escape") closePopup();
  });
}

async function start() {
  wire();
  await listen("state-changed", (event) => render(event.payload));
  await listen("popup-opened", resetPopup);
  await listen("popup-close-request", closePopup);
  const initial = inTauri
    ? await invoke("get_state")
    : {
        device_name: "CMF BUDS PRO 2",
        connected: false,
        status: "Not connected",
        port: "",
        firmware: "",
        left: { level: 0, charging: false, present: false },
        right: { level: 0, charging: false, present: false },
        case_battery: { level: 0, charging: false, present: false },
        anc_mode: "off",
        anc_strength: 1,
        eq_preset: 0,
        in_ear_detection: true,
        autostart: false,
      };
  render(initial);
}

start();
