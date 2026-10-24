/**
 * Native menu. Every notebook-level item just forwards a command string to the
 * focused renderer, which owns the document model and does the actual work.
 */

import { BrowserWindow, Menu, type MenuItemConstructorOptions, app, shell } from 'electron'

function send(command: string): void {
  const window = BrowserWindow.getFocusedWindow() ?? BrowserWindow.getAllWindows()[0]
  window?.webContents.send('menu:command', command)
}

function item(label: string, command: string, accelerator?: string): MenuItemConstructorOptions {
  return { label, accelerator, click: () => send(command) }
}

export function buildMenu(): void {
  const isMac = process.platform === 'darwin'

  const template: MenuItemConstructorOptions[] = [
    ...(isMac
      ? ([
          {
            label: app.name,
            submenu: [
              { role: 'about' },
              { type: 'separator' },
              { role: 'services' },
              { type: 'separator' },
              { role: 'hide' },
              { role: 'hideOthers' },
              { role: 'unhide' },
              { type: 'separator' },
              { role: 'quit' },
            ],
          },
        ] as MenuItemConstructorOptions[])
      : []),
    {
      label: 'File',
      submenu: [
        item('New Notebook', 'notebook.new', 'CmdOrCtrl+N'),
        item('Open Notebook…', 'notebook.open', 'CmdOrCtrl+O'),
        item('Open Folder…', 'workspace.openFolder', 'CmdOrCtrl+Shift+O'),
        { type: 'separator' },
        item('Save', 'notebook.save', 'CmdOrCtrl+S'),
        item('Save As…', 'notebook.saveAs', 'CmdOrCtrl+Shift+S'),
        { type: 'separator' },
        item('Close Tab', 'tab.close', 'CmdOrCtrl+W'),
        isMac ? { role: 'close' } : { role: 'quit' },
      ],
    },
    {
      label: 'Edit',
      submenu: [
        { role: 'undo' },
        { role: 'redo' },
        { type: 'separator' },
        { role: 'cut' },
        { role: 'copy' },
        { role: 'paste' },
        { role: 'selectAll' },
        { type: 'separator' },
        item('Quick Open…', 'workspace.quickOpen', 'CmdOrCtrl+P'),
        item('Command Palette…', 'app.commandPalette', 'CmdOrCtrl+Shift+P'),
      ],
    },
    {
      label: 'Cell',
      submenu: [
        item('Run Cell', 'cell.run', 'CmdOrCtrl+Return'),
        item('Run Cell and Select Below', 'cell.runAndAdvance', 'Shift+Return'),
        item('Run Cell and Insert Below', 'cell.runAndInsertBelow', 'Alt+Return'),
        item('Run All Cells', 'cell.runAll'),
        { type: 'separator' },
        // A/B mirror the single-key command-mode shortcuts; Cmd+Shift+Up/Down
        // would shadow macOS's select-to-start/end inside the editor.
        item('Insert Code Cell Above', 'cell.insertAbove', 'CmdOrCtrl+Shift+A'),
        item('Insert Code Cell Below', 'cell.insertBelow', 'CmdOrCtrl+Shift+B'),
        item('Insert Markdown Cell Below', 'cell.insertMarkdownBelow', 'CmdOrCtrl+Shift+M'),
        item('Delete Cell', 'cell.delete'),
        { type: 'separator' },
        item('Change to Code', 'cell.toCode'),
        item('Change to Markdown', 'cell.toMarkdown'),
        { type: 'separator' },
        item('Clear Outputs', 'cell.clearOutputs'),
        item('Clear All Outputs', 'notebook.clearAllOutputs'),
      ],
    },
    {
      label: 'Kernel',
      submenu: [
        item('Interrupt Kernel', 'kernel.interrupt', 'CmdOrCtrl+.'),
        item('Restart Kernel', 'kernel.restart'),
        item('Restart and Run All', 'kernel.restartRunAll'),
        { type: 'separator' },
        item('Change Kernel…', 'kernel.change'),
        item('Shut Down Kernel', 'kernel.shutdown'),
      ],
    },
    {
      label: 'View',
      submenu: [
        item('Toggle Sidebar', 'view.toggleSidebar', 'CmdOrCtrl+B'),
        { type: 'separator' },
        { role: 'resetZoom' },
        { role: 'zoomIn' },
        { role: 'zoomOut' },
        { type: 'separator' },
        { role: 'togglefullscreen' },
        { role: 'toggleDevTools' },
        { role: 'reload' },
      ],
    },
    {
      role: 'window',
      submenu: [{ role: 'minimize' }, { role: 'zoom' }, ...(isMac ? [{ role: 'front' as const }] : [])],
    },
    {
      role: 'help',
      submenu: [
        {
          label: 'Jupyter Messaging Spec',
          click: () => shell.openExternal('https://jupyter-client.readthedocs.io/en/stable/messaging.html'),
        },
      ],
    },
  ]

  Menu.setApplicationMenu(Menu.buildFromTemplate(template))
}
