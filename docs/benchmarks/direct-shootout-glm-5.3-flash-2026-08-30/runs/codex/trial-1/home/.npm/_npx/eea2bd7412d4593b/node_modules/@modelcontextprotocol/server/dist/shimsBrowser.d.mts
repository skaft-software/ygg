import { t as CfWorkerJsonSchemaValidator } from "./cfWorkerProvider-C03CSigr.mjs";

//#region src/shimsBrowser.d.ts

declare const process: {
  readonly stdin: never;
  readonly stdout: never;
};
//#endregion
export { CfWorkerJsonSchemaValidator as DefaultJsonSchemaValidator, process };
//# sourceMappingURL=shimsBrowser.d.mts.map