import React from "react";
import { useTranslation } from "react-i18next";
import { ToggleSwitch } from "../ui/ToggleSwitch";
import { useSettings } from "../../hooks/useSettings";
import { useOsType } from "../../hooks/useOsType";
import { useModelStore } from "../../stores/modelStore";

interface LiveTypingProps {
  descriptionMode?: "inline" | "tooltip";
  grouped?: boolean;
}

export const LiveTyping: React.FC<LiveTypingProps> = React.memo(
  ({ descriptionMode = "tooltip", grouped = false }) => {
    const { t } = useTranslation();
    const { getSetting, updateSetting, isUpdating } = useSettings();
    const osType = useOsType();
    const { currentModel, models } = useModelStore();
    const enabled = getSetting("live_typing_enabled") ?? false;
    const supportsStreaming =
      models.find((m) => m.id === currentModel)?.supports_streaming ?? false;

    // Checks the field through the macOS Accessibility API; needs a streaming model.
    if (osType !== "macos" || !supportsStreaming) {
      return null;
    }

    return (
      <ToggleSwitch
        checked={enabled}
        onChange={(nextEnabled) =>
          updateSetting("live_typing_enabled", nextEnabled)
        }
        isUpdating={isUpdating("live_typing_enabled")}
        label={t("settings.advanced.liveTyping.title")}
        description={t("settings.advanced.liveTyping.description")}
        descriptionMode={descriptionMode}
        grouped={grouped}
      />
    );
  },
);
