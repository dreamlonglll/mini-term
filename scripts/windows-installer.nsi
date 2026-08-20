; Mini-Term GPUI 版 Windows NSIS 安装器(release.yml 的 Windows 线用 makensis 编译)。
;
; 身份对齐旧 Tauri NSIS(currentUser 模式):卸载注册表键沿用
; HKCU\Software\Microsoft\Windows\CurrentVersion\Uninstall\Mini-Term,并经
; InstallDirRegKey 读旧 InstallLocation —— 老用户运行新安装器默认落回原目录,
; 同名键覆盖写入,原地升级不留双条目(旧 Tauri 的 uninstall.exe 也被本包的
; 覆盖,注册表里那条 UninstallString 始终指向在场的卸载器)。
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
!macroend

Section "Install"
  !insertmacro KILL_RUNNING

  SetOutPath "$INSTDIR"
  File "${SOURCE_DIR}\mini-term.exe"
  File "${SOURCE_DIR}\miniterm-hook.exe"
  File "${SOURCE_DIR}\mt-ssh-cli.exe"
  File "${SOURCE_DIR}\mt-ssh-mcp.exe"
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
  RMDir /r "$INSTDIR\portable-conpty"
  Delete "$INSTDIR\uninstall.exe"
  ; 只删空目录:用户自选目录里若有别的东西,不动。
  RMDir "$INSTDIR"

  Delete "$SMPROGRAMS\${PRODUCT_NAME}.lnk"
  Delete "$DESKTOP\${PRODUCT_NAME}.lnk"
  DeleteRegKey HKCU "${UNINST_KEY}"
SectionEnd
