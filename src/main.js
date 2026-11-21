const core = window.__TAURI__?.core;
const events = window.__TAURI__?.event;

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
  port: el("device-port"),
  firmware: el("device-firmware"),
  pill: el("anc-pill"),
  strengthRow: el("strength-row"),
  inEar: el("in-ear"),
};

const ANC_ORDER = ["anc", "transparency", "off"];

let state = null;

function invoke(name, args = {}) {
  if (core) return core.invoke(name, args);
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

function battery(value, barId, valueId, chargeId) {
  const present = !!value?.present;
  el(barId).style.width = present ? `${Math.max(0, Math.min(100, value.level))}%` : "0%";
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
  ui.status.textContent = next.status || (next.connected ? "Connected" : "Disconnected");
  ui.port.textContent = next.port || "";
  ui.firmware.textContent = next.firmware ? `FW ${next.firmware}` : "";

  battery(next.left, "bar-left", "battery-left", "charge-left");
  battery(next.right, "bar-right", "battery-right", "charge-right");
  battery(next.case_battery, "bar-case", "battery-case", "charge-case");

  const index = Math.max(0, ANC_ORDER.indexOf(next.anc_mode));
  ui.pill.style.setProperty("--anc-index", index);
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
  el("close").addEventListener("click", () => invoke("hide_popup"));
  ui.connectButton.addEventListener("click", () =>
    invoke(state?.connected ? "disconnect" : "connect"),
  );

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

  ui.inEar.addEventListener("change", () =>
    invoke("set_in_ear", { enabled: ui.inEar.checked }),
  );

  document.addEventListener("keydown", (event) => {
    if (event.key === "Escape") invoke("hide_popup");
  });
}

async function start() {
  wire();
  if (events?.listen) {
    await events.listen("state-changed", (event) => render(event.payload));
  }
  const initial = core
    ? await core.invoke("get_state")
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
