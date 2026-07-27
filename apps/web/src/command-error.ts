import type { CommandAck, CommandErrorCode } from "./protocol";

export class CommandRejectedError extends Error {
  readonly code?: CommandErrorCode;
  readonly retryable: boolean;
  readonly currentGeneration?: number;

  constructor(
    message: string,
    options: {
      code?: CommandErrorCode;
      retryable?: boolean;
      currentGeneration?: number;
    } = {},
  ) {
    super(message);
    this.name = "CommandRejectedError";
    this.code = options.code;
    this.retryable = options.retryable ?? false;
    this.currentGeneration = options.currentGeneration;
  }
}

export function rejectedCommandError(
  ack: CommandAck,
  fallback: string,
): CommandRejectedError {
  return new CommandRejectedError(ack.error ?? fallback, {
    code: ack.errorCode,
    retryable: ack.retryable,
    currentGeneration: ack.currentGeneration,
  });
}
