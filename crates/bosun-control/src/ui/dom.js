// The pane's elements, looked up once by id, and the width check the sheets
// use.

export {
  $,
  activityLog,
  askChips,
  askFree,
  askInput,
  askSheet,
  btnAskBack,
  btnAskDismiss,
  btnAskSend,
  btnAskType,
  btnBack,
  btnBottom,
  btnBrowse,
  btnChildCollapse,
  btnClear,
  btnCopyId,
  btnDirClose,
  btnDirUp,
  btnDirUse,
  btnFork,
  btnInterrupt,
  btnMachinesBack,
  btnMcpAdd,
  btnMcpBack,
  btnMcpCancel,
  btnMcpSubmit,
  btnMore,
  btnPermission,
  btnPersona,
  btnRefToggle,
  btnSend,
  btnSheetClose,
  btnSkillRepoAdd,
  btnSkillRepoCancel,
  btnSkillsAdd,
  btnSkillsBack,
  btnStop,
  btnWatchOpen,
  cancelNewSessionBtn,
  chatRow,
  childList,
  childPanel,
  childPanelDot,
  childPanelTitle,
  childTranscript,
  dirBrowser,
  dirEntries,
  dirPathEl,
  formDir,
  formMcpAuth,
  formMcpBearer,
  formMcpName,
  formMcpOAuthClient,
  formMcpOAuthSecret,
  formMcpUrl,
  formNode,
  formPersona,
  formSkillRef,
  formSkillRepo,
  healthDot,
  healthStrip,
  healthText,
  input,
  inputRow,
  isNarrow,
  machinesList,
  machinesTab,
  mcpAdd,
  mcpBearerLabel,
  mcpChips,
  mcpList,
  mcpOAuthClientLabel,
  mcpOAuthSecretLabel,
  mcpStrip,
  mcpStripText,
  mcpTab,
  newSessionBtn,
  newSessionForm,
  nodeChips,
  originEl,
  personaChips,
  personaName,
  recentList,
  rowClear,
  rowFork,
  rowInterrupt,
  rowPermission,
  rowPersona,
  rowStop,
  sessionListEl,
  skillRefLabel,
  skillsAdd,
  skillsList,
  skillsStrip,
  skillsStripText,
  skillsTab,
  statusEl,
  toastEl,
  transcript,
  view,
  viewClear,
  viewDir,
  viewFork,
  viewIdCopy,
  viewNode,
  viewPermission,
  viewSheet,
  viewSheetMeta,
  viewStateDot,
  viewTitle,
  viewWaiting,
  watchBanner,
};

const $ = (id) => document.getElementById(id);

// Thin-client narrow viewport check; the pane keeps the same sheets at any
// width, only the focus/scroll behavior differs on small screens.
const isNarrow = () => window.innerWidth <= 640;

const originEl = $('origin');
const statusEl = $('status');
const healthStrip = $('health-strip');
const healthDot = $('health-dot');
const healthText = $('health-text');
const toastEl = $('toast');
const machinesTab = $('machines-tab');
const btnMachinesBack = $('btn-machines-back');
const machinesList = $('machines-list');
const skillsStrip = $('skills-strip');
const skillsStripText = $('skills-strip-text');
const skillsTab = $('skills-tab');
const btnSkillsBack = $('btn-skills-back');
const btnSkillsAdd = $('btn-skills-add');
const skillsList = $('skills-list');
const skillsAdd = $('skills-add');
const formSkillRepo = $('form-skill-repo');
const formSkillRef = $('form-skill-ref');
const skillRefLabel = $('skill-ref-label');
const btnRefToggle = $('btn-ref-toggle');
const btnSkillRepoAdd = $('btn-skill-repo-add');
const btnSkillRepoCancel = $('btn-skill-repo-cancel');
const mcpStrip = $('mcp-strip');
const mcpStripText = $('mcp-strip-text');
const mcpTab = $('mcp-tab');
const btnMcpBack = $('btn-mcp-back');
const btnMcpAdd = $('btn-mcp-add');
const mcpList = $('mcp-list');
const mcpAdd = $('mcp-add');
const formMcpName = $('form-mcp-name');
const formMcpUrl = $('form-mcp-url');
const formMcpAuth = $('form-mcp-auth');
const formMcpBearer = $('form-mcp-bearer');
const mcpBearerLabel = $('mcp-bearer-label');
const formMcpOAuthClient = $('form-mcp-oauth-client');
const formMcpOAuthSecret = $('form-mcp-oauth-secret');
const mcpOAuthClientLabel = $('mcp-oauth-client-label');
const mcpOAuthSecretLabel = $('mcp-oauth-secret-label');
const btnMcpSubmit = $('btn-mcp-submit');
const btnMcpCancel = $('btn-mcp-cancel');
const sessionListEl = $('session-list');
const newSessionBtn = $('btn-new-session');
const newSessionForm = $('new-session-form');
const cancelNewSessionBtn = $('btn-cancel-new-session');
const nodeChips = $('node-chips');
const formNode = $('form-node');
const formDir = $('form-dir');
const btnBrowse = $('btn-browse');
const dirBrowser = $('dir-browser');
const dirPathEl = $('dir-path');
const btnDirUp = $('btn-dir-up');
const btnDirUse = $('btn-dir-use');
const btnDirClose = $('btn-dir-close');
const dirEntries = $('dir-entries');
const personaChips = $('persona-chips');
const formPersona = $('form-persona');
const mcpChips = $('mcp-chips');
const recentList = $('recent-sessions');
const view = $('session-view');
const viewStateDot = $('view-state-dot');

const viewNode = $('view-node');
const viewDir = $('view-dir');
const viewTitle = $('view-title');
const viewWaiting = $('view-waiting');
const activityLog = $('activity-log');
const btnBack = $('btn-back');
const viewSheet = $('view-sheet');
const btnMore = $('btn-more');
const btnSheetClose = $('btn-sheet-close');
const btnCopyId = $('btn-copy-id');
const viewIdCopy = $('view-id-copy');
const watchBanner = $('watch-banner');
const btnWatchOpen = $('btn-watch-open');
const viewPermission = $('view-permission');
const viewSheetMeta = $('view-sheet-meta');
const rowPermission = $('row-permission');
const rowPersona = $('row-persona');
const rowFork = $('row-fork');
const btnFork = $('btn-fork');
const viewFork = $('view-fork');
const rowClear = $('row-clear');
const btnClear = $('btn-clear');
const viewClear = $('view-clear');
const rowInterrupt = $('row-interrupt');
const rowStop = $('row-stop');
const inputRow = $('input-row');
const btnInterrupt = $('btn-interrupt');
const btnPermission = $('btn-permission');
const personaName = $('persona-name');
const btnPersona = $('btn-persona');
const btnStop = $('btn-stop');
const transcript = $('transcript');
const btnBottom = $('btn-bottom');
const childPanel = $('child-panel');
const childTranscript = $('child-transcript');
const childPanelTitle = $('child-panel-title');
const childList = $('child-list');
const childPanelDot = $('child-panel-dot');
const btnChildCollapse = $('btn-child-collapse');
const input = $('input');
const chatRow = $('chat-row');
const btnSend = $('btn-send');
const askSheet = $('ask-sheet');
const askChips = $('ask-chips');
const btnAskDismiss = $('btn-ask-dismiss');
const btnAskType = $('btn-ask-type');
const askFree = $('ask-free');
const askInput = $('ask-input');
const btnAskSend = $('btn-ask-send');
const btnAskBack = $('btn-ask-back');
