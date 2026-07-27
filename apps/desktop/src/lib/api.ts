import { commands, type CommandError, type MaskRule } from "@/bindings";

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
  startBackup: (...args: Parameters<typeof commands.startBackup>) =>
    unwrap(commands.startBackup(...args)),
  startRestore: (...args: Parameters<typeof commands.startRestore>) =>
    unwrap(commands.startRestore(...args)),
  backupDirectory: () => unwrap(commands.backupDirectory()),
  listArtifacts: (directory: string | null = null) =>
    unwrap(commands.listArtifacts(directory)),
  checkArtifact: (path: string) => unwrap(commands.checkArtifact(path)),
  deleteArtifact: (path: string) => unwrap(commands.deleteArtifact(path)),
  startSync: (...args: Parameters<typeof commands.startSync>) =>
    unwrap(commands.startSync(...args)),
  listSyncPlans: (profileId: string) => unwrap(commands.listSyncPlans(profileId)),
  createSyncPlan: (...args: Parameters<typeof commands.createSyncPlan>) =>
    unwrap(commands.createSyncPlan(...args)),
  updateSyncPlan: (...args: Parameters<typeof commands.updateSyncPlan>) =>
    unwrap(commands.updateSyncPlan(...args)),
  deleteSyncPlan: (id: string) => unwrap(commands.deleteSyncPlan(id)),
  importTablesConf: (contents: string) =>
    unwrap(commands.importTablesConf(contents)),
  listJobs: (limit: number) => unwrap(commands.listJobs(limit)),
  listAudit: (limit: number) => unwrap(commands.listAudit(limit)),
  cancelJob: (id: string) => unwrap(commands.cancelJob(id)),
  appInfo: () => unwrap(commands.appInfo()),

  listSchedules: () => unwrap(commands.listSchedules()),
  getSchedule: (id: string) => unwrap(commands.getSchedule(id)),
  createSchedule: (...args: Parameters<typeof commands.createSchedule>) =>
    unwrap(commands.createSchedule(...args)),
  updateSchedule: (...args: Parameters<typeof commands.updateSchedule>) =>
    unwrap(commands.updateSchedule(...args)),
  deleteSchedule: (id: string) => unwrap(commands.deleteSchedule(id)),
  runScheduleNow: (id: string) => unwrap(commands.runScheduleNow(id)),
  previewCron: (...args: Parameters<typeof commands.previewCron>) =>
    unwrap(commands.previewCron(...args)),
  crontabLine: (id: string) => unwrap(commands.crontabLine(id)),
  schedulerStatus: () => unwrap(commands.schedulerStatus()),
  getAppSettings: () => unwrap(commands.getAppSettings()),
  setAppSettings: (...args: Parameters<typeof commands.setAppSettings>) =>
    unwrap(commands.setAppSettings(...args)),
  cliStatus: () => unwrap(commands.cliStatus()),
  installCli: () => unwrap(commands.installCli()),

  setSyncPlanMasking: (id: string, masking: MaskRule[]) =>
    unwrap(commands.setSyncPlanMasking(id, masking)),
  maskingPreview: (planId: string) => unwrap(commands.maskingPreview(planId)),

  backupKeyStatus: () => unwrap(commands.backupKeyStatus()),
  generateBackupKey: () => unwrap(commands.generateBackupKey()),
  setBackupKeyRecipients: (keys: string[]) =>
    unwrap(commands.setBackupKeyRecipients(keys)),
  /** Writes the secret to a file and returns the path. It never returns the key. */
  exportBackupKeyToFile: () => unwrap(commands.exportBackupKeyToFile()),

  /** Writes a bundle to a file and returns the path. It carries no secrets. */
  exportConfigToFile: () => unwrap(commands.exportConfigToFile()),
  previewConfigImport: (path: string) =>
    unwrap(commands.previewConfigImport(path)),
  importConfig: (path: string) => unwrap(commands.importConfig(path)),

  libraryStats: (directory: string | null = null) =>
    unwrap(commands.libraryStats(directory)),

  listDestinations: () => unwrap(commands.listDestinations()),
  /** The secret goes in and is never returned; it lands in the OS keychain. */
  createDestination: (...args: Parameters<typeof commands.createDestination>) =>
    unwrap(commands.createDestination(...args)),
  updateDestination: (...args: Parameters<typeof commands.updateDestination>) =>
    unwrap(commands.updateDestination(...args)),
  setDestinationCredential: (id: string, secret: string) =>
    unwrap(commands.setDestinationCredential(id, secret)),
  deleteDestination: (id: string) => unwrap(commands.deleteDestination(id)),
  testDestination: (id: string) => unwrap(commands.testDestination(id)),
  pushArtifactOffsite: (path: string) =>
    unwrap(commands.pushArtifactOffsite(path)),
};
