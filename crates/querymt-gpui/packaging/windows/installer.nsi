; Per-user NSIS installer for QueryMT Desktop (no administrator rights needed).
;
;   makensis -DVERSION=0.1.0 -DSRC_EXE=<path\querymt-gpui.exe> \
;            -DQMTCODE_EXE=<path\qmtcode.exe> \
;            -DICON=<path\icon.ico> -DOUT_FILE=<path\setup.exe> installer.nsi
;
; QMTCODE_EXE is the qmtcode of the same release; it is installed next to the
; app. Release builds always pass it (scripts/package/windows.sh and CI check
; that). Without it, the installer is built with a warning, for local runs.
;
; Setup flags:
;   /S                 silent install
;   /UPDATE            used by the app's self-update: wait for the running app
;                      to exit, install silently and start the new version
;   /D=<dir>           install directory (must be the last argument)

Unicode true

!ifndef VERSION
  !error "VERSION is not defined"
!endif
!ifndef SRC_EXE
  !error "SRC_EXE is not defined"
!endif
!ifndef ICON
  !error "ICON is not defined"
!endif
!ifndef OUT_FILE
  !error "OUT_FILE is not defined"
!endif
!ifndef QMTCODE_EXE
  !warning "QMTCODE_EXE is not defined: the installer has no bundled qmtcode"
!endif

!define APP_NAME "QueryMT Desktop"
!define APP_EXE "querymt-gpui.exe"
!define QMTCODE_NAME "qmtcode.exe"
; Distinct from the key the Tauri installer uses (product name "QueryMT Desktop"),
; so neither uninstaller removes the other.
!define UNINSTALL_KEY "Software\Microsoft\Windows\CurrentVersion\Uninstall\QueryMTDesktopGpui"
; The earlier "QueryMT GPUI" install, removed silently by this installer.
!define OLD_UNINSTALL_KEY "Software\Microsoft\Windows\CurrentVersion\Uninstall\querymt-gpui"

Name "${APP_NAME}"
OutFile "${OUT_FILE}"
InstallDir "$LOCALAPPDATA\Programs\QueryMT Desktop"
InstallDirRegKey HKCU "${UNINSTALL_KEY}" "InstallLocation"
RequestExecutionLevel user
SetCompressor /SOLID lzma

!include "MUI2.nsh"
!include "FileFunc.nsh"
!include "LogicLib.nsh"
!insertmacro GetParameters
!insertmacro GetOptions

!define MUI_ICON "${ICON}"
!define MUI_UNICON "${ICON}"
!define MUI_FINISHPAGE_RUN "$INSTDIR\${APP_EXE}"

!insertmacro MUI_PAGE_DIRECTORY
!insertmacro MUI_PAGE_INSTFILES
!insertmacro MUI_PAGE_FINISH
!insertmacro MUI_UNPAGE_CONFIRM
!insertmacro MUI_UNPAGE_INSTFILES
!insertmacro MUI_LANGUAGE "English"

; 1 when started with /UPDATE.
Var IsUpdate

; Pushes "1" when the file does not exist or can be opened for writing, "0"
; when something (the running app or its agent) holds it. Opening for append
; fails while a process has the executable mapped, and does not change it.
Function FileIsFree
  Pop $R1
  IfFileExists "$R1" 0 free
  ClearErrors
  FileOpen $R0 "$R1" a
  IfErrors busy
  FileClose $R0
  free:
    Push 1
    Return
  busy:
    Push 0
FunctionEnd

; Waits up to 30 s for the app and qmtcode to exit, then gives up with the old
; version intact (nothing has been changed at this point).
Function WaitUntilFree
  StrCpy $R2 0
  retry:
    Push "$INSTDIR\${APP_EXE}"
    Call FileIsFree
    Pop $R3
    Push "$INSTDIR\${QMTCODE_NAME}"
    Call FileIsFree
    Pop $R4
    StrCmp "$R3$R4" "11" done
    IntOp $R2 $R2 + 1
    IntCmp $R2 30 timeout 0 timeout
    Sleep 1000
    Goto retry
  timeout:
    MessageBox MB_OK|MB_ICONSTOP "${APP_NAME} is still running. Close it and run the update again." /SD IDOK
    Abort
  done:
FunctionEnd

Function .onInit
  ${GetParameters} $0
  ClearErrors
  ${GetOptions} $0 "/UPDATE" $1
  ${If} ${Errors}
    StrCpy $IsUpdate 0
  ${Else}
    StrCpy $IsUpdate 1
    Call WaitUntilFree
  ${EndIf}
FunctionEnd

Section "Install"
  ; Remove an earlier "QueryMT GPUI" install: its own uninstaller, silently
  ; (`_?=` runs it in place and waits), then what it leaves behind.
  ReadRegStr $0 HKCU "${OLD_UNINSTALL_KEY}" "UninstallString"
  ReadRegStr $1 HKCU "${OLD_UNINSTALL_KEY}" "InstallLocation"
  ${If} $0 != ""
    ClearErrors
    ExecWait '$0 /S _?=$1' $2
    ; Only a clean removal lets go of the old registration. If it could not
    ; run, or failed, its uninstaller and entry stay, so the old app can still
    ; be removed from Windows' Apps list; this install goes on beside it.
    ${If} ${Errors}
    ${OrIf} $2 != 0
      DetailPrint "The earlier QueryMT GPUI could not be removed (exit code $2); it is left in place."
    ${Else}
      ${If} $1 != ""
        Delete "$1\uninstall.exe"
        RMDir "$1"
      ${EndIf}
      DeleteRegKey HKCU "${OLD_UNINSTALL_KEY}"
    ${EndIf}
  ${EndIf}

  SetOutPath "$INSTDIR"
  File "/oname=${APP_EXE}" "${SRC_EXE}"
!ifdef QMTCODE_EXE
  File "/oname=${QMTCODE_NAME}" "${QMTCODE_EXE}"
!endif
  WriteUninstaller "$INSTDIR\uninstall.exe"
  CreateShortcut "$SMPROGRAMS\${APP_NAME}.lnk" "$INSTDIR\${APP_EXE}"

  WriteRegStr HKCU "${UNINSTALL_KEY}" "DisplayName" "${APP_NAME}"
  WriteRegStr HKCU "${UNINSTALL_KEY}" "DisplayVersion" "${VERSION}"
  WriteRegStr HKCU "${UNINSTALL_KEY}" "Publisher" "QueryMT"
  WriteRegStr HKCU "${UNINSTALL_KEY}" "DisplayIcon" "$INSTDIR\${APP_EXE}"
  WriteRegStr HKCU "${UNINSTALL_KEY}" "InstallLocation" "$INSTDIR"
  WriteRegStr HKCU "${UNINSTALL_KEY}" "UninstallString" '"$INSTDIR\uninstall.exe"'
  WriteRegDWORD HKCU "${UNINSTALL_KEY}" "NoModify" 1
  WriteRegDWORD HKCU "${UNINSTALL_KEY}" "NoRepair" 1

  ; Self-update: start the new version.
  ${If} $IsUpdate == 1
    Exec '"$INSTDIR\${APP_EXE}"'
  ${EndIf}
SectionEnd

Section "Uninstall"
  Delete "$INSTDIR\${APP_EXE}"
  Delete "$INSTDIR\${QMTCODE_NAME}"
  Delete "$INSTDIR\uninstall.exe"
  RMDir "$INSTDIR"
  Delete "$SMPROGRAMS\${APP_NAME}.lnk"
  DeleteRegKey HKCU "${UNINSTALL_KEY}"
SectionEnd
