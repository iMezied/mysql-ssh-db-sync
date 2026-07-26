import { commands, type CommandError } from "@/bindings";

/**
 * tauri-specta returns a discriminated result rather than throwing. Unwrap it
 * into a rejected promise so TanStack Query's error states work normally.
 */
export async function unwrap<T>(
  call: Promise<{ status: "ok"; data: T } | { status: "error"; error: CommandError }>,
): Promise<T> {
  const result = await call;
  if (result.status === "error") {
    throw new ApiError(result.error);
  }
  return result.data;
}

export class ApiError extends Error {
  readonly kind: string;

  constructor(error: CommandError) {
    super(error.message);
    this.name = "ApiError";
    this.kind = error.kind;
  }
}

export const api = {
  listProfiles: () => unwrap(commands.listProfiles()),
  getProfile: (id: string) => unwrap(commands.getProfile(id)),
  createProfile: (...args: Parameters<typeof commands.createProfile>) =>
    unwrap(commands.createProfile(...args)),
  updateProfile: (...args: Parameters<typeof commands.updateProfile>) =>
    unwrap(commands.updateProfile(...args)),
  deleteProfile: (id: string) => unwrap(commands.deleteProfile(id)),
  setProfileSecret: (id: string, kind: string, value: string) =>
    unwrap(commands.setProfileSecret(id, kind, value)),
  profileSecretStatus: (id: string) => unwrap(commands.profileSecretStatus(id)),
  testConnection: (id: string) => unwrap(commands.testConnection(id)),
  trustHostKey: (
    hostPort: string,
    algorithm: string,
    fingerprint: string,
    replace: boolean,
  ) => unwrap(commands.trustHostKey(hostPort, algorithm, fingerprint, replace)),
  listDatabases: (id: string) => unwrap(commands.listDatabases(id)),
  listTables: (id: string, database: string) =>
    unwrap(commands.listTables(id, database)),
  listJobs: (limit: number) => unwrap(commands.listJobs(limit)),
  cancelJob: (id: string) => unwrap(commands.cancelJob(id)),
  appInfo: () => unwrap(commands.appInfo()),
};
