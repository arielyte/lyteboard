import { invoke } from "@tauri-apps/api/core";

export type DaemonStatus = {
  running: boolean;
  version: string;
  pid: number;
  uptimeSeconds: number;
};
export type DiscoveryState = "off" | "loopback" | "running" | "unavailable";
export type Listener = {
  address: string;
  discovery: DiscoveryState;
  loopback: boolean;
};
export type Device = {
  id: string;
  name: string;
  address: string | null;
  state: "online" | "offline" | "connecting";
};
export type DevicesView = { listener: Listener; devices: Device[] };
export type NearbyDevice = { id: string; address: string; paired: boolean };
export type Delivery = { id: string; error: string | null };
export type Invitation = {
  text: string;
  expiresAt: number;
  size: number;
  modules: string;
};
export type PairState =
  | { state: "idle" | "connecting" | "approving" | "paired" }
  | { state: "compare"; code: string }
  | { state: "failed"; error: string };

export const discoveryLabels: Record<DiscoveryState, string> = {
  off: "Off",
  loopback: "Paused on loopback",
  running: "On",
  unavailable: "Unavailable",
};

export const api = {
  daemon: () => invoke<DaemonStatus | null>("daemon_status"),
  devices: () => invoke<DevicesView>("devices_snapshot"),
  discover: () => invoke<NearbyDevice[]>("discover_devices"),
  setListener: (endpoint: string) =>
    invoke<Listener>("set_listener", { endpoint }),
  setDiscovery: (enabled: boolean) =>
    invoke<Listener>("set_discovery", { enabled }),
  rename: (id: string, name: string) =>
    invoke<void>("rename_device", { id, name }),
  setAddress: (id: string, endpoint: string) =>
    invoke<void>("set_device_address", { id, endpoint }),
  remove: (id: string) => invoke<void>("remove_device", { id }),
  send: (recipients: string[]) =>
    invoke<Delivery[]>("send_clipboard", { recipients }),
  invite: () => invoke<Invitation>("pair_invite"),
  importInvitation: (invitation: string) =>
    invoke<void>("pair_import", { invitation }),
  pairStatus: () => invoke<PairState>("pair_status"),
  confirmPair: (code: string) => invoke<void>("pair_confirm", { code }),
  cancelPair: () => invoke<void>("pair_cancel"),
};

export function errorMessage(error: unknown): string {
  return typeof error === "string"
    ? error
    : error instanceof Error
      ? error.message
      : "The operation failed. Try again.";
}
