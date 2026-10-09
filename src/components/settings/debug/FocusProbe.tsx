import React from "react";
import { useTranslation } from "react-i18next";
import { useOsType } from "../../../hooks/useOsType";
import { useFocusProbe } from "../../../hooks/useFocusProbe";

const PROBE_DELAY_MS = 5000;

/**
 * Temporary diagnostic (macOS): reads metadata of the focused field in the
 * app you switch to during the delay. Never shows the field's text.
 */
export const FocusProbe: React.FC = () => {
  const { t } = useTranslation();
  const osType = useOsType();
  const { running, probe, error, run } = useFocusProbe(PROBE_DELAY_MS);

  if (osType !== "macos") {
    return null;
  }

  return (
    <div className="p-4 space-y-2">
      <div className="flex justify-between items-center gap-2">
        <div>
          <p className="text-sm font-medium">
            {t("settings.debug.focusProbe.title")}
          </p>
          <p className="text-xs text-mid-gray">
            {t("settings.debug.focusProbe.description")}
          </p>
        </div>
        <button
          onClick={run}
          disabled={running}
          className="px-2 py-1 text-sm font-semibold bg-mid-gray/10 border border-mid-gray/80 hover:bg-logo-primary/10 rounded cursor-pointer hover:border-logo-primary disabled:opacity-50 disabled:cursor-default whitespace-nowrap"
        >
          {t("settings.debug.focusProbe.run")}
        </button>
      </div>
      {running && (
        <p className="text-sm animate-pulse">
          {t("settings.debug.focusProbe.running")}
        </p>
      )}
      {error !== null && (
        <p className="text-sm text-red-500">
          {t("settings.debug.focusProbe.failed", { error })}
        </p>
      )}
      {probe !== null && (
        <dl className="text-sm font-mono grid grid-cols-[auto_1fr] gap-x-4">
          {Object.entries(probe).map(([key, value]) => (
            <React.Fragment key={key}>
              <dt className="text-mid-gray">{key}</dt>
              <dd>{JSON.stringify(value)}</dd>
            </React.Fragment>
          ))}
        </dl>
      )}
    </div>
  );
};
