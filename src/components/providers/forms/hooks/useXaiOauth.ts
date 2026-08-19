import { useManagedAuth } from "./useManagedAuth";

/** xAI OAuth device-code authentication hook. */
export function useXaiOauth(options?: { pollStatus?: boolean }) {
  return useManagedAuth("xai_oauth", options);
}
