export const ccConnectImport = {
  zh: {
    placeholderNote:
      "导入的项目会附带一个占位 Telegram 平台(假凭据),稍后请到 Dashboard 替换为真实 IM 平台(请替换而非直接删除占位平台,否则该项目会缺平台导致 cc-connect 下次无法启动)。",
    unknownError: "未知错误",
    importTitle: "导入到 cc-connect",
    importConfirm:
      "将向 cc-connect 添加项目「{name}」并重启 cc-connect,可能短暂中断现有 IM 会话,继续吗?\n\n{note}\n\n工作目录: {path}\nAgent 类型: claudecode (后续可在 Dashboard 中修改)",
    importRestartFailedTitle: "导入成功但 cc-connect 重启失败",
    importRestartFailedMsg:
      "项目「{name}」已写入 cc-connect 配置;但重启 cc-connect 失败:\n{error}\n\n下次启动 cc-connect 时新项目会生效。",
    importFailedTitle: "导入失败",
    importFailedNoWrite: "cc-connect 未能写入项目配置",
    batchListMore: "…等共 {count} 个",
    batchImportTitle: "批量导入到 cc-connect",
    batchImportConfirm:
      "将向 cc-connect 添加以下 {count} 个项目并重启一次 cc-connect,可能短暂中断现有 IM 会话,继续吗?\n\n{list}\n\n{note}",
    batchRestartFailedTitle: "批量导入成功但 cc-connect 重启失败",
    batchRestartFailedMsg:
      "{count} 个项目已写入 cc-connect 配置;但重启 cc-connect 失败:\n{error}\n\n下次启动 cc-connect 时新项目会生效。",
    batchNoNeedTitle: "无需导入",
    batchNoNeedMsg: "选中的项目在 cc-connect 中均已存在",
    batchImportFailedTitle: "批量导入失败",
    notImportedTitle: "未导入",
    notImportedMsg: "项目「{name}」尚未导入到 cc-connect",
    removeTitle: "从 cc-connect 移除",
    removeConfirm:
      "将从 cc-connect 删除项目「{name}」并重启 cc-connect,可能短暂中断现有 IM 会话,继续吗?",
    deleteFailedTitle: "cc-connect 删除失败",
    deleteFailedConfirm: "{error}\n\n是否仍要从 mini-term 端清理「{name}」的导入记录?",
    removeRestartFailedTitle: "移除成功但 cc-connect 重启失败",
    removeRestartFailedMsg:
      "项目「{name}」已从 cc-connect 删除;但重启 cc-connect 失败:\n{error}\n\n下次启动 cc-connect 时会生效。",
  },
  en: {
    placeholderNote:
      "Imported projects come with a placeholder Telegram platform (fake credentials); replace it with a real IM platform in the Dashboard later (replace it rather than deleting it outright, otherwise the project will have no platform and cc-connect will fail to start next time).",
    unknownError: "Unknown error",
    importTitle: "Import to cc-connect",
    importConfirm:
      "This will add project \"{name}\" to cc-connect and restart cc-connect, which may briefly interrupt active IM sessions. Continue?\n\n{note}\n\nWorking directory: {path}\nAgent type: claudecode (can be changed later in the Dashboard)",
    importRestartFailedTitle: "Imported, but cc-connect restart failed",
    importRestartFailedMsg:
      "Project \"{name}\" was written to the cc-connect config, but restarting cc-connect failed:\n{error}\n\nThe new project will take effect the next time cc-connect starts.",
    importFailedTitle: "Import failed",
    importFailedNoWrite: "cc-connect failed to write the project config",
    batchListMore: "… and {count} in total",
    batchImportTitle: "Batch import to cc-connect",
    batchImportConfirm:
      "This will add the following {count} projects to cc-connect and restart cc-connect once, which may briefly interrupt active IM sessions. Continue?\n\n{list}\n\n{note}",
    batchRestartFailedTitle: "Batch import succeeded, but cc-connect restart failed",
    batchRestartFailedMsg:
      "{count} projects were written to the cc-connect config, but restarting cc-connect failed:\n{error}\n\nThe new projects will take effect the next time cc-connect starts.",
    batchNoNeedTitle: "Nothing to import",
    batchNoNeedMsg: "All selected projects already exist in cc-connect",
    batchImportFailedTitle: "Batch import failed",
    notImportedTitle: "Not imported",
    notImportedMsg: "Project \"{name}\" has not been imported to cc-connect yet",
    removeTitle: "Remove from cc-connect",
    removeConfirm:
      "This will delete project \"{name}\" from cc-connect and restart cc-connect, which may briefly interrupt active IM sessions. Continue?",
    deleteFailedTitle: "cc-connect deletion failed",
    deleteFailedConfirm:
      "{error}\n\nDo you still want to clean up the import record for \"{name}\" on the mini-term side?",
    removeRestartFailedTitle: "Removed, but cc-connect restart failed",
    removeRestartFailedMsg:
      "Project \"{name}\" was deleted from cc-connect, but restarting cc-connect failed:\n{error}\n\nThis will take effect the next time cc-connect starts.",
  },
} as const;
