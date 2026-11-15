; Glotzkiste per-user Windows setup (NSIS 3). Built by scripts/package.ps1:
;   makensis /DVERSION=1.0.0 /DVI_VERSION=1.0.0.0 /DSRC=<dir> /DOUTFILE=<file> installer\glotzkiste.nsi
; SRC holds glotzkiste.exe (libmpv embedded), LICENSE.txt,
; THIRD-PARTY-NOTICES.txt, libmpv-NOTICE.txt and LGPL-2.1.txt.
;
; Installs to %LOCALAPPDATA%\Programs\Glotzkiste without admin rights, with a
; Start menu shortcut (and a desktop one if chosen) and an Installed apps
; entry. No PATH changes, no file types.
;
; The in-app updater runs this setup with /S /UPDATE: it waits for Glotzkiste
; to close, installs, and starts it again. Uninstall removes only installed
; files: the layout (%APPDATA%\Glotzkiste), caches (%LOCALAPPDATA%\Glotzkiste)
; and the login in Credential Manager stay, so a reinstall keeps everything.

!macro REQUIRE name
  !ifndef ${name}
    !error "Pass /D${name}=..."
  !endif
!macroend
!insertmacro REQUIRE VERSION
!insertmacro REQUIRE VI_VERSION
!insertmacro REQUIRE SRC
!insertmacro REQUIRE OUTFILE

Unicode true
ManifestDPIAware true
RequestExecutionLevel user
SetCompressor /SOLID lzma

!include MUI2.nsh
!include LogicLib.nsh
!include FileFunc.nsh

!define UNINSTALL_KEY "Software\Microsoft\Windows\CurrentVersion\Uninstall\Glotzkiste"

Name "Glotzkiste"
OutFile "${OUTFILE}"
InstallDir "$LOCALAPPDATA\Programs\Glotzkiste"
InstallDirRegKey HKCU "${UNINSTALL_KEY}" "InstallLocation"
BrandingText "Glotzkiste ${VERSION}"

VIProductVersion "${VI_VERSION}"
VIAddVersionKey "ProductName" "Glotzkiste"
VIAddVersionKey "ProductVersion" "${VERSION}"
VIAddVersionKey "FileVersion" "${VERSION}"
VIAddVersionKey "FileDescription" "Glotzkiste ${VERSION} setup"
VIAddVersionKey "LegalCopyright" "MIT License"

!define MUI_ICON "..\assets\logo\glotzkiste.ico"
!define MUI_UNICON "..\assets\logo\glotzkiste.ico"
!define MUI_ABORTWARNING
!define MUI_FINISHPAGE_RUN "$INSTDIR\glotzkiste.exe"
!define MUI_FINISHPAGE_RUN_TEXT "Start Glotzkiste"
; The second finish-page checkbox adds a desktop shortcut.
!define MUI_FINISHPAGE_SHOWREADME ""
!define MUI_FINISHPAGE_SHOWREADME_TEXT "Put a shortcut on the desktop"
!define MUI_FINISHPAGE_SHOWREADME_NOTCHECKED
!define MUI_FINISHPAGE_SHOWREADME_FUNCTION DesktopShortcut

!insertmacro MUI_PAGE_LICENSE "${SRC}\LICENSE.txt"
!insertmacro MUI_PAGE_DIRECTORY
!insertmacro MUI_PAGE_INSTFILES
!insertmacro MUI_PAGE_FINISH
!insertmacro MUI_UNPAGE_CONFIRM
!insertmacro MUI_UNPAGE_INSTFILES
!insertmacro MUI_LANGUAGE "English"

Var Update

Function .onInit
  StrCpy $Update 0
  ${GetParameters} $0
  ClearErrors
  ${GetOptions} $0 "/UPDATE" $1
  ${IfNot} ${Errors}
    StrCpy $Update 1
  ${EndIf}
FunctionEnd

Function DesktopShortcut
  CreateShortcut "$DESKTOP\Glotzkiste.lnk" "$INSTDIR\glotzkiste.exe"
FunctionEnd

; A running glotzkiste.exe cannot be replaced. Interactive runs ask to close
; it; silent runs (the updater) wait up to 30 s for it to exit, then fail.
!macro WAIT_FOR_APP un
Function ${un}WaitForApp
  StrCpy $2 0
  retry:
  ${IfNot} ${FileExists} "$INSTDIR\glotzkiste.exe"
    Return
  ${EndIf}
  ClearErrors
  FileOpen $0 "$INSTDIR\glotzkiste.exe" a
  ${IfNot} ${Errors}
    FileClose $0
    Return
  ${EndIf}
  IfSilent wait
  MessageBox MB_RETRYCANCEL|MB_ICONEXCLAMATION "Glotzkiste is running. Close every Glotzkiste window, then choose Retry." /SD IDCANCEL IDRETRY retry
  SetErrorLevel 2
  Quit
  wait:
  ${If} $2 >= 60
    SetErrorLevel 2
    Quit
  ${EndIf}
  Sleep 500
  IntOp $2 $2 + 1
  Goto retry
FunctionEnd
!macroend
!insertmacro WAIT_FOR_APP ""
!insertmacro WAIT_FOR_APP "un."

Section "Glotzkiste"
  Call WaitForApp
  SetOutPath "$INSTDIR"
  File "${SRC}\glotzkiste.exe"
  File "${SRC}\LICENSE.txt"
  File "${SRC}\THIRD-PARTY-NOTICES.txt"
  File "${SRC}\libmpv-NOTICE.txt"
  File "${SRC}\LGPL-2.1.txt"
  WriteUninstaller "$INSTDIR\uninstall.exe"
  CreateShortcut "$SMPROGRAMS\Glotzkiste.lnk" "$INSTDIR\glotzkiste.exe"

  WriteRegStr HKCU "${UNINSTALL_KEY}" "DisplayName" "Glotzkiste"
  WriteRegStr HKCU "${UNINSTALL_KEY}" "DisplayVersion" "${VERSION}"
  WriteRegStr HKCU "${UNINSTALL_KEY}" "Publisher" "rennerdo30"
  WriteRegStr HKCU "${UNINSTALL_KEY}" "URLInfoAbout" "https://github.com/rennerdo30/glotzkiste"
  WriteRegStr HKCU "${UNINSTALL_KEY}" "DisplayIcon" "$INSTDIR\glotzkiste.exe,0"
  WriteRegStr HKCU "${UNINSTALL_KEY}" "InstallLocation" "$INSTDIR"
  WriteRegStr HKCU "${UNINSTALL_KEY}" "UninstallString" '"$INSTDIR\uninstall.exe"'
  WriteRegStr HKCU "${UNINSTALL_KEY}" "QuietUninstallString" '"$INSTDIR\uninstall.exe" /S'
  WriteRegDWORD HKCU "${UNINSTALL_KEY}" "NoModify" 1
  WriteRegDWORD HKCU "${UNINSTALL_KEY}" "NoRepair" 1
  ${GetSize} "$INSTDIR" "/S=0K" $0 $1 $2
  IntFmt $0 "0x%08X" $0
  WriteRegDWORD HKCU "${UNINSTALL_KEY}" "EstimatedSize" "$0"
  ${If} $Update == 1
    Exec '"$INSTDIR\glotzkiste.exe"'
  ${EndIf}
SectionEnd

Section "Uninstall"
  Call un.WaitForApp
  Delete "$INSTDIR\glotzkiste.exe"
  Delete "$INSTDIR\LICENSE.txt"
  Delete "$INSTDIR\THIRD-PARTY-NOTICES.txt"
  Delete "$INSTDIR\libmpv-NOTICE.txt"
  Delete "$INSTDIR\LGPL-2.1.txt"
  Delete "$INSTDIR\uninstall.exe"
  RMDir "$INSTDIR"
  Delete "$SMPROGRAMS\Glotzkiste.lnk"
  Delete "$DESKTOP\Glotzkiste.lnk"
  DeleteRegKey HKCU "${UNINSTALL_KEY}"
SectionEnd
