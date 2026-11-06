import React from "react";
import { useTranslation } from "react-i18next";
import { ToggleSwitch } from "../ui/ToggleSwitch";
import { useSettings } from "../../hooks/useSettings";
import { useOsType } from "../../hooks/useOsType";

interface AutoLearnProps {
  descriptionMode?: "inline" | "tooltip";
  grouped?: boolean;
}

export const AutoLearn: React.FC<AutoLearnProps> = React.memo(
  ({ descriptionMode = "tooltip", grouped = false }) => {
    const { t } = useTranslation();
    const { getSetting, updateSetting, isUpdating } = useSettings();
    const osType = useOsType();
    const enabled = getSetting("auto_learn_enabled") ?? false;

    // Reads the focused field through the macOS Accessibility API.
    if (osType !== "macos") {
      return null;
    }

    return (
      <>
        <ToggleSwitch
          checked={enabled}
          onChange={(nextEnabled) =>
            updateSetting("auto_learn_enabled", nextEnabled)
          }
          isUpdating={isUpdating("auto_learn_enabled")}
          label={t("settings.advanced.autoLearn.title")}
          description={t("settings.advanced.autoLearn.description")}
          descriptionMode={descriptionMode}
          grouped={grouped}
        />
        {enabled && (
          <ToggleSwitch
            checked={getSetting("auto_learn_llm_filter") ?? false}
            onChange={(nextEnabled) =>
              updateSetting("auto_learn_llm_filter", nextEnabled)
            }
            isUpdating={isUpdating("auto_learn_llm_filter")}
            label={t("settings.advanced.autoLearnLlmFilter.title")}
            description={t("settings.advanced.autoLearnLlmFilter.description")}
            descriptionMode={descriptionMode}
            grouped={grouped}
          />
        )}
      </>
    );
  },
);
