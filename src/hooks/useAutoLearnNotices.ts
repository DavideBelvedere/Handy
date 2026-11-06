import { useEffect } from "react";
import { useTranslation } from "react-i18next";
import { listen } from "@tauri-apps/api/event";
import { commands } from "@/bindings";
import { useNoticeStore } from "@/stores/noticeStore";

// Auto-learn reports what it learned (or a custom word that misfired) up to a
// minute after a dictation. Neither is a failure, so they are passing toasts,
// not backend notices that would stay in the tray.
export const useAutoLearnNotices = () => {
  const { t } = useTranslation();

  useEffect(() => {
    const notify = useNoticeStore.getState().notify;
    const undo = async (from: string, to: string) => {
      const result = await commands.undoLearnedWord(from, to);
      if (result.status === "ok" && result.data) {
        notify({
          title: t("autoLearn.undone", { from, to }),
          level: "warning",
        });
      } else {
        // false: the pair was already removed, or edited by hand since.
        notify({
          title: t("autoLearn.undoFailed", { from, to }),
          detail: result.status === "error" ? result.error : undefined,
        });
      }
    };
    const unlistenLearned = listen<{ from: string; to: string }>(
      "word-learned",
      ({ payload: { from, to } }) => {
        notify({
          title: t("autoLearn.learned", { from, to }),
          level: "warning",
          action: { label: t("autoLearn.undo"), onClick: () => undo(from, to) },
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
