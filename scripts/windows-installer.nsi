; Mini-Term GPUI 版 Windows NSIS 安装器(release.yml 的 Windows 线用 makensis 编译)。
;
; 身份对齐旧 Tauri NSIS(currentUser 模式):卸载注册表键沿用
; HKCU\Software\Microsoft\Windows\CurrentVersion\Uninstall\Mini-Term,并经
; InstallDirRegKey 读旧 InstallLocation —— 老用户运行新安装器默认落回原目录,
; 同名键覆盖写入,原地升级不留双条目(旧 Tauri 的 uninstall.exe 也被本包的
; 覆盖,注册表里那条 UninstallString 始终指向在场的卸载器)。
;
; 升级不再是「文件覆盖写」:启动时读注册表认出已装版本 → 弹框告知要先卸载
; (取消即退出安装器) → 用户在安装页按下开始后,先跑旧版自己的卸载器,再铺新
; 文件。这样旧版有、新版没有的残留文件不会留在安装目录里。用户数据在 AppData
; 下,卸载器不碰,升级不丢配置。详见 UNINSTALL_OLD 宏的注释。
;
; 包内布局 = 运行时布局:mini-term.exe + 三个 sidecar + portable-conpty\ 全部
; 平铺 $INSTDIR,与便携解压、target\<profile>\ 开发布局同构(「与 exe 同目录」
; 定位铁律)。用户数据在 AppData 下,卸载不碰。
;
; 编译期必须 /D 传入(全部绝对路径):
;   VERSION      完整语义版本(如 1.0.0-beta,进注册表 DisplayVersion)
;   VERSION_NUM  纯数字四段(如 1.0.0.0,VIProductVersion 只收这个)
;   SOURCE_DIR   产物目录(target\release,已由 stage-sidecars.mjs 就位齐)
;   ICON_FILE    安装器图标(crates\mt-app\resources\icon.ico)
;   OUT_FILE     产物 setup.exe 输出路径

Unicode true
!include "MUI2.nsh"
!include "FileFunc.nsh"
!include "LogicLib.nsh"

!ifndef VERSION
  !error "makensis 需要 /DVERSION=<semver>"
!endif
!ifndef VERSION_NUM
  !error "makensis 需要 /DVERSION_NUM=<x.y.z.w>"
!endif
!ifndef SOURCE_DIR
  !error "makensis 需要 /DSOURCE_DIR=<target\release 绝对路径>"
!endif
!ifndef ICON_FILE
  !error "makensis 需要 /DICON_FILE=<icon.ico 绝对路径>"
!endif
!ifndef OUT_FILE
  !error "makensis 需要 /DOUT_FILE=<setup.exe 输出路径>"
!endif

!define PRODUCT_NAME "Mini-Term"
!define UNINST_KEY "Software\Microsoft\Windows\CurrentVersion\Uninstall\${PRODUCT_NAME}"

; 已装旧版的现场(.onInit 探到,Section 用):空串 = 没装过,走全新安装。
Var OldUninstaller
Var OldInstallDir
Var OldVersion
Var UninstStatus

Name "${PRODUCT_NAME}"
OutFile "${OUT_FILE}"
; 用户级安装,无 UAC —— 与旧 Tauri currentUser 模式一致;默认目录也取旧版
; 默认值($LOCALAPPDATA\Mini-Term),装过旧版的经 InstallDirRegKey 回原目录。
RequestExecutionLevel user
InstallDir "$LOCALAPPDATA\${PRODUCT_NAME}"
InstallDirRegKey HKCU "${UNINST_KEY}" "InstallLocation"
SetCompressor /SOLID lzma
ManifestDPIAware true

VIProductVersion "${VERSION_NUM}"
VIAddVersionKey /LANG=1033 "ProductName" "${PRODUCT_NAME}"
VIAddVersionKey /LANG=1033 "ProductVersion" "${VERSION}"
VIAddVersionKey /LANG=1033 "FileVersion" "${VERSION_NUM}"
VIAddVersionKey /LANG=1033 "FileDescription" "${PRODUCT_NAME} Installer"
VIAddVersionKey /LANG=1033 "LegalCopyright" "mini-term"

!define MUI_ICON "${ICON_FILE}"
!define MUI_UNICON "${ICON_FILE}"
!define MUI_ABORTWARNING

!insertmacro MUI_PAGE_DIRECTORY
!insertmacro MUI_PAGE_INSTFILES
!define MUI_FINISHPAGE_RUN "$INSTDIR\mini-term.exe"
!insertmacro MUI_PAGE_FINISH
!insertmacro MUI_UNPAGE_CONFIRM
!insertmacro MUI_UNPAGE_INSTFILES

; 双语:运行时按系统界面语言自动挑选,不弹选择框。
!insertmacro MUI_LANGUAGE "English"
!insertmacro MUI_LANGUAGE "SimpChinese"

; 升级路径的三条文案(LangString 必须排在 MUI_LANGUAGE 之后;串里的 $Var 运行时展开)。
LangString MSG_OLD_FOUND ${LANG_ENGLISH} "${PRODUCT_NAME} $OldVersion is already installed in:$\r$\n$OldInstallDir$\r$\n$\r$\nIt will be uninstalled first, then ${VERSION} will be installed. Your settings and data (under AppData) are kept.$\r$\n$\r$\nClick OK to continue, or Cancel to quit."
LangString MSG_OLD_FOUND ${LANG_SIMPCHINESE} "检测到已安装 ${PRODUCT_NAME} $OldVersion,位置:$\r$\n$OldInstallDir$\r$\n$\r$\n将先卸载它,再安装 ${VERSION}。你的配置与数据(在 AppData 下)不会被删除。$\r$\n$\r$\n点「确定」继续,点「取消」退出安装。"
LangString MSG_UNINST_RUN ${LANG_ENGLISH} "Uninstalling ${PRODUCT_NAME} $OldVersion from $OldInstallDir ..."
LangString MSG_UNINST_RUN ${LANG_SIMPCHINESE} "正在卸载旧版 ${PRODUCT_NAME} $OldVersion($OldInstallDir)..."
LangString MSG_UNINST_FAIL ${LANG_ENGLISH} "The old version was not fully removed (uninstaller exit code: $UninstStatus).$\r$\n$\r$\nInstall ${VERSION} anyway (overwriting the existing files)?"
LangString MSG_UNINST_FAIL ${LANG_SIMPCHINESE} "旧版本没有卸载干净(卸载器退出码:$UninstStatus)。$\r$\n$\r$\n仍要继续安装 ${VERSION} 吗(直接覆盖现有文件)?"

; 升级前放倒在跑的实例:主程序锁着 exe 没法覆盖;mt-ssh-cli 的 daemon 与
; hook 常驻同理(旧 Tauri 版主程序叫 Mini-Term.exe,taskkill 不分大小写,
; 同一条命令连旧版一起管住)。没在跑时 taskkill 报错,吞掉即可。
!macro KILL_RUNNING
  nsExec::Exec 'taskkill /F /IM mini-term.exe'
  Pop $0
  nsExec::Exec 'taskkill /F /IM miniterm-hook.exe'
  Pop $0
  nsExec::Exec 'taskkill /F /IM mt-ssh-cli.exe'
  Pop $0
  nsExec::Exec 'taskkill /F /IM mt-ssh-mcp.exe'
  Pop $0
  ; mt-agent-cli 是短命进程(编排者调一次跑一次),但升级时它恰好在跑就会锁住
  ; 文件 —— 与 miniterm-hook 同一个理由,一并放倒。
  nsExec::Exec 'taskkill /F /IM mt-agent-cli.exe'
  Pop $0
!macroend

; 跑旧版自己的卸载器。带 `_?=` 是关键:没有它,NSIS 卸载器会先把自己复制到
; %TEMP% 再启动,ExecWait 等到的是那个立即返回的壳,新文件会和卸载动作打架。
; 代价是运行中的 uninstall.exe 删不掉自己,残留由本宏补删(旧 Tauri 版的卸载器
; 同样是 NSIS 出身,`_?=` 与 /S 都认)。
; 卸载器会清掉快捷方式与 Uninstall 注册表键,本 Section 后半段原样重建。
!macro UNINSTALL_OLD
  ${If} $OldUninstaller != ""
    DetailPrint "$(MSG_UNINST_RUN)"
    ClearErrors
    ExecWait '"$OldUninstaller" /S _?=$OldInstallDir' $UninstStatus
    ${If} ${Errors}
      StrCpy $UninstStatus "-1"
    ${EndIf}
    Delete "$OldUninstaller"
    ; 旧目录若已空就收掉;用户改选了新目录时这一步顺带清干净老位置。
    RMDir "$OldInstallDir"
    ${If} $UninstStatus != "0"
      ; 卸载失败不直接判死:静默安装默认继续覆盖,交互装由用户定。
      MessageBox MB_YESNO|MB_ICONEXCLAMATION "$(MSG_UNINST_FAIL)" /SD IDYES IDYES +2
        Abort
    ${EndIf}
  ${EndIf}
!macroend

; 认出已装版本并征得同意。放 .onInit 是为了让用户在第一屏就知道要先卸载;真正
; 的卸载动作留到 Section(用户在目录页仍可反悔,反悔时旧版原封不动)。
Function .onInit
  ReadRegStr $0 HKCU "${UNINST_KEY}" "UninstallString"
  ${If} $0 == ""
    Return
  ${EndIf}
  ; 注册表里的 UninstallString 是带引号的命令行,剥成裸路径才能喂 ExecWait。
  StrCpy $1 $0 1
  StrCpy $2 $0 "" -1
  ${If} $1 == '"'
  ${AndIf} $2 == '"'
    StrCpy $0 $0 -1 1
  ${EndIf}
  ${IfNot} ${FileExists} "$0"
    ; 注册表在、卸载器不在(用户手删过目录):当全新安装处理,不拿假提示烦人。
    Return
  ${EndIf}
  StrCpy $OldUninstaller "$0"
  ; `_?=` 要的是卸载器自己所在的目录,所以取它的父目录而不是注册表里的
  ; InstallLocation(那条可能带尾反斜杠或被手工改过)。
  ${GetParent} "$0" $OldInstallDir
  ${If} $OldInstallDir == ""
    ReadRegStr $OldInstallDir HKCU "${UNINST_KEY}" "InstallLocation"
  ${EndIf}
  ReadRegStr $OldVersion HKCU "${UNINST_KEY}" "DisplayVersion"
  ${If} $OldVersion == ""
    StrCpy $OldVersion "?"
  ${EndIf}
  ${IfNot} ${Silent}
    MessageBox MB_OKCANCEL|MB_ICONINFORMATION "$(MSG_OLD_FOUND)" /SD IDOK IDOK +2
      Abort
  ${EndIf}
FunctionEnd

Section "Install"
  ; 先放倒实例再卸载:旧卸载器同样要动这几个 exe。
  !insertmacro KILL_RUNNING
  !insertmacro UNINSTALL_OLD

  SetOutPath "$INSTDIR"
  File "${SOURCE_DIR}\mini-term.exe"
  File "${SOURCE_DIR}\miniterm-hook.exe"
  File "${SOURCE_DIR}\mt-ssh-cli.exe"
  File "${SOURCE_DIR}\mt-ssh-mcp.exe"
  File "${SOURCE_DIR}\mt-agent-cli.exe"
  SetOutPath "$INSTDIR\portable-conpty"
  File /r "${SOURCE_DIR}\portable-conpty\*"
  SetOutPath "$INSTDIR"

  WriteUninstaller "$INSTDIR\uninstall.exe"
  CreateShortcut "$SMPROGRAMS\${PRODUCT_NAME}.lnk" "$INSTDIR\mini-term.exe"
  CreateShortcut "$DESKTOP\${PRODUCT_NAME}.lnk" "$INSTDIR\mini-term.exe"

  WriteRegStr HKCU "${UNINST_KEY}" "DisplayName" "${PRODUCT_NAME}"
  WriteRegStr HKCU "${UNINST_KEY}" "DisplayVersion" "${VERSION}"
  WriteRegStr HKCU "${UNINST_KEY}" "DisplayIcon" "$INSTDIR\mini-term.exe"
  WriteRegStr HKCU "${UNINST_KEY}" "Publisher" "mini-term"
  WriteRegStr HKCU "${UNINST_KEY}" "InstallLocation" "$INSTDIR"
  WriteRegStr HKCU "${UNINST_KEY}" "UninstallString" '"$INSTDIR\uninstall.exe"'
  WriteRegDWORD HKCU "${UNINST_KEY}" "NoModify" 1
  WriteRegDWORD HKCU "${UNINST_KEY}" "NoRepair" 1
  ${GetSize} "$INSTDIR" "/S=0K" $0 $1 $2
  IntFmt $0 "0x%08X" $0
  WriteRegDWORD HKCU "${UNINST_KEY}" "EstimatedSize" $0
SectionEnd

Section "Uninstall"
  !insertmacro KILL_RUNNING

  Delete "$INSTDIR\mini-term.exe"
  Delete "$INSTDIR\miniterm-hook.exe"
  Delete "$INSTDIR\mt-ssh-cli.exe"
  Delete "$INSTDIR\mt-ssh-mcp.exe"
  Delete "$INSTDIR\mt-agent-cli.exe"
  RMDir /r "$INSTDIR\portable-conpty"
  Delete "$INSTDIR\uninstall.exe"
  ; 只删空目录:用户自选目录里若有别的东西,不动。
  RMDir "$INSTDIR"

  Delete "$SMPROGRAMS\${PRODUCT_NAME}.lnk"
  Delete "$DESKTOP\${PRODUCT_NAME}.lnk"
  DeleteRegKey HKCU "${UNINST_KEY}"
SectionEnd
