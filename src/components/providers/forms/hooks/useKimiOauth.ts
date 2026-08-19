import { useManagedAuth } from "./useManagedAuth";

export function useKimiOauth(options?: { pollStatus?: boolean }) {
  return useManagedAuth("kimi_oauth", options);
}
