import { useManagedAuth } from "./useManagedAuth";

export function useKimiOauth() {
  return useManagedAuth("kimi_oauth");
}
