import React from "react";
import { useTranslation } from "react-i18next";
import { ToggleSwitch } from "../ui/ToggleSwitch";
import { useSettings } from "../../hooks/useSettings";

interface ScratchThatProps {
  descriptionMode?: "inline" | "tooltip";
  grouped?: boolean;
}

export const ScratchThat: React.FC<ScratchThatProps> = React.memo(
  ({ descriptionMode = "tooltip", grouped = false }) => {
    const { t } = useTranslation();
    const { getSetting, updateSetting, isUpdating } = useSettings();
    const enabled = getSetting("scratch_that_enabled") ?? true;

    return (
      <ToggleSwitch
        checked={enabled}
        onChange={(nextEnabled) =>
          updateSetting("scratch_that_enabled", nextEnabled)
        }
        isUpdating={isUpdating("scratch_that_enabled")}
        label={t("settings.advanced.scratchThat.title")}
        description={t("settings.advanced.scratchThat.description")}
        descriptionMode={descriptionMode}
        grouped={grouped}
      />
    );
  },
);
