import { useEffect } from "react";
import { useTranslation } from "react-i18next";
import { listen } from "@tauri-apps/api/event";
import { useNoticeStore } from "@/stores/noticeStore";

// Auto-learn reports what it learned (or a custom word that misfired) up to a
// minute after a dictation. Neither is a failure, so they are passing toasts,
// not backend notices that would stay in the tray.
export const useAutoLearnNotices = () => {
  const { t } = useTranslation();

  useEffect(() => {
    const notify = useNoticeStore.getState().notify;
    const unlistenLearned = listen<{ from: string; to: string }>(
      "word-learned",
      (event) => {
        notify({
          title: t("autoLearn.learned", event.payload),
          level: "warning",
        });
      },
    );
    const unlistenMisfire = listen<{ word: string }>(
      "custom-word-misfire",
      (event) => {
        notify({
          title: t("autoLearn.misfire", event.payload),
          level: "warning",
        });
      },
    );
    return () => {
      unlistenLearned.then((fn) => fn());
      unlistenMisfire.then((fn) => fn());
    };
  }, [t]);
};
