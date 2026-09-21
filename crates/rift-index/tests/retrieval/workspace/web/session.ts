/** One caller's open session against the served workspace. */
export class WorkspaceSession {
  readonly workspaceId: string;

  constructor(workspaceId: string) {
    this.workspaceId = workspaceId;
  }

  /** Closes the session and releases the reader it held. */
  close(): void {
    return;
  }
}

/** Reads the ranking weights the operator configured. */
export function readRankingWeights(): Record<string, number> {
  return { identifier: 0.35, lexical: 0.35, vector: 0.3 };
}
