import React from "react";
import { useTranslation } from "react-i18next";
import { ToggleSwitch } from "../ui/ToggleSwitch";
import { useSettings } from "../../hooks/useSettings";
import { useOsType } from "../../hooks/useOsType";

interface WindowContextVocabularyProps {
  descriptionMode?: "inline" | "tooltip";
  grouped?: boolean;
}

export const WindowContextVocabulary: React.FC<WindowContextVocabularyProps> =
  React.memo(({ descriptionMode = "tooltip", grouped = false }) => {
    const { t } = useTranslation();
    const { getSetting, updateSetting, isUpdating } = useSettings();
    const osType = useOsType();

    // Reads the focused field through the macOS Accessibility API.
    if (osType !== "macos") {
      return null;
    }

    return (
      <ToggleSwitch
        checked={getSetting("window_context_vocabulary") ?? false}
        onChange={(enabled) =>
          updateSetting("window_context_vocabulary", enabled)
        }
        isUpdating={isUpdating("window_context_vocabulary")}
        label={t("settings.advanced.windowContextVocabulary.title")}
        description={t("settings.advanced.windowContextVocabulary.description")}
        descriptionMode={descriptionMode}
        grouped={grouped}
      />
    );
  });
