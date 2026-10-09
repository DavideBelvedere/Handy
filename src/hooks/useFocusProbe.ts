import { useState } from "react";
import { commands, type FocusProbe } from "@/bindings";

/** Runs the temporary AX focused-field probe; the backend waits `delayMs` before reading. */
export const useFocusProbe = (delayMs: number) => {
  const [running, setRunning] = useState(false);
  const [probe, setProbe] = useState<FocusProbe | null>(null);
  const [error, setError] = useState<string | null>(null);

  const run = async () => {
    setRunning(true);
    setProbe(null);
    setError(null);
    try {
      const result = await commands.probeFocusedField(delayMs);
      if (result.status === "ok") {
        setProbe(result.data);
      } else {
        setError(result.error);
      }
    } catch (e) {
      setError(String(e));
    } finally {
      setRunning(false);
    }
  };

  return { running, probe, error, run };
};
