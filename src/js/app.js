
const $ = s => document.querySelector( s );
let CONFIG = { master_url: '', vault_url: '' };
let PACKAGES = {};
let currentPkg = null, currentVer = null;

function showStatus ( msg, ok ) {
  const s = $( '#status' );
  s.textContent = msg;
  s.className = ok ? 'ok' : 'err';
  s.style.display = 'block';
}
function fmtSize ( n ) {
  if ( n < 1024 ) return n + ' B';
  if ( n < 1048576 ) return ( n / 1024 ).toFixed( 1 ) + ' KB';
  return ( n / 1048576 ).toFixed( 1 ) + ' MB';
}
function esc ( s ) { return String( s ).replace( /[&<>"]/g, c => ( { '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;' }[ c ] ) ); }

// ── tabs ──
document.querySelectorAll( '.tab' ).forEach( t => t.addEventListener( 'click', () => {
  document.querySelectorAll( '.tab' ).forEach( x => x.classList.remove( 'active' ) );
  t.classList.add( 'active' );
  $( '#tab-browse' ).classList.toggle( 'hidden', t.dataset.tab !== 'browse' );
  $( '#tab-checklist' ).classList.toggle( 'hidden', t.dataset.tab !== 'checklist' );
  $( '#tab-repos' ).classList.toggle( 'hidden', t.dataset.tab !== 'repos' );
  if ( t.dataset.tab === 'upload' ) openUploadModal();
  if ( t.dataset.tab === 'checklist' ) {
    if ( CL.pass ) loadChecklist();
    else $( '#cl-password' ).focus();
  }
  if ( t.dataset.tab === 'repos' ) openRepos();
} ) );

// ── private checklist ──
const CL = { pass: sessionStorage.getItem( 'clpass' ) || '', items: [] };
async function loadChecklist () {
  if ( !CL.pass ) return lockChecklist();
  try {
    const r = await fetch( '/api/checklist', { headers: { 'X-Api-Password': CL.pass } } );
    if ( r.status === 401 ) return lockChecklist();
    if ( !r.ok ) return showStatus( 'Failed to load checklist', false );
    CL.items = ( await r.json() ).items || [];
    $( '#checklist-lock' ).classList.add( 'hidden' );
    $( '#checklist-body' ).classList.remove( 'hidden' );
    renderChecklist();
  } catch { showStatus( 'Network error loading checklist', false ); }
}
function unlockChecklist () {
  const pass = $( '#cl-password' ).value;
  fetch( '/api/checklist', { headers: { 'X-Api-Password': pass } } ).then( async r => {
    if ( r.status === 401 ) {
      const box = $( '#cl-lock-err' ); box.classList.remove( 'hidden' ); box.textContent = 'Wrong password.';
      return;
    }
    if ( !r.ok ) {
      const box = $( '#cl-lock-err' ); box.classList.remove( 'hidden' ); box.textContent = 'Error loading checklist.';
      return;
    }
    CL.pass = pass;
    sessionStorage.setItem( 'clpass', pass );
    CL.items = ( await r.json() ).items || [];
    $( '#cl-lock-err' ).classList.add( 'hidden' );
    $( '#checklist-lock' ).classList.add( 'hidden' );
    $( '#checklist-body' ).classList.remove( 'hidden' );
    renderChecklist();
  } );
}
function lockChecklist () {
  CL.pass = ''; sessionStorage.removeItem( 'clpass' );
  $( '#checklist-body' ).classList.add( 'hidden' );
  $( '#checklist-lock' ).classList.remove( 'hidden' );
  $( '#cl-lock-err' ).classList.add( 'hidden' );
  $( '#cl-password' ).value = '';
}
function renderChecklist () {
  const box = $( '#cl-items' );
  if ( !CL.items.length ) {
    box.innerHTML = '<div class="empty" style="padding:12px">Nothing to upload yet — add something below.</div>';
    return;
  }
  box.innerHTML = CL.items.map( ( it, i ) => `
    <div class="cl-item${ it.done ? ' done' : '' }">
      <input type="checkbox" class="cl-check" ${ it.done ? 'checked' : '' } data-i="${ i }">
      <input class="cl-text" value="${ esc( it.text ) }" data-i="${ i }" autocomplete="off">
      <button class="ghost cl-del" data-i="${ i }" title="remove">✕</button>
    </div>`).join( '' );
  box.querySelectorAll( '.cl-check' ).forEach( c => c.onchange = () => {
    CL.items[ +c.dataset.i ].done = c.checked; renderChecklist();
  } );
  box.querySelectorAll( '.cl-text' ).forEach( t => t.oninput = () => {
    CL.items[ +t.dataset.i ].text = t.value;
  } );
  box.querySelectorAll( '.cl-del' ).forEach( b => b.onclick = () => {
    CL.items.splice( +b.dataset.i, 1 ); renderChecklist();
  } );
}
function saveChecklist () {
  const hint = $( '#cl-saved-hint' ); hint.textContent = 'saving…';
  fetch( '/api/checklist', {
    method: 'POST',
    headers: { 'Content-Type': 'application/json', 'X-Api-Password': CL.pass },
    body: JSON.stringify( { items: CL.items } ),
  } ).then( async r => {
    const j = await r.json();
    hint.textContent = j.ok ? 'saved ✓' : ( j.error || 'save failed' );
    if ( !j.ok ) showStatus( j.error || 'save failed', false );
    else setTimeout( () => hint.textContent = '', 2000 );
  } ).catch( () => { hint.textContent = 'error'; showStatus( 'Network error saving checklist', false ); } );
}
$( '#cl-unlock' ).onclick = unlockChecklist;
$( '#cl-password' ).addEventListener( 'keydown', e => { if ( e.key === 'Enter' ) unlockChecklist(); } );
$( '#cl-add' ).onclick = () => {
  const v = $( '#cl-new' ).value.trim();
  if ( !v ) return;
  CL.items.push( { text: v, done: false } );
  $( '#cl-new' ).value = ''; renderChecklist(); $( '#cl-new' ).focus();
};
$( '#cl-new' ).addEventListener( 'keydown', e => { if ( e.key === 'Enter' ) $( '#cl-add' ).click(); } );
$( '#cl-save' ).onclick = saveChecklist;
$( '#cl-clear-done' ).onclick = () => { CL.items = CL.items.filter( i => !i.done ); renderChecklist(); };
$( '#cl-lock' ).onclick = lockChecklist;

// ── upload modal ──
function openUploadModal ( pkgName ) {
  $( '#upload-modal' ).classList.remove( 'hidden' );
  if ( pkgName ) {
    $( '#up-name' ).value = pkgName;
    $( '#up-version' ).value = '';
    refreshUploadMeta();
    $( '#up-version' ).focus();
  } else {
    $( '#up-name' ).focus();
  }
}
function closeUploadModal () {
  $( '#upload-modal' ).classList.add( 'hidden' );
}

// ── categories (loaded from the server, which reads <shop>/categories.txt) ──
async function loadCategories () {
  try {
    const r = await fetch( '/api/categories' );
    if ( !r.ok ) return;
    const cats = await r.json();
    const sel = $( '#up-category' );
    sel.innerHTML = cats.map( c => `<option${ c === 'Misc' ? ' selected' : '' }>${ esc( c ) }</option>` ).join( '' );
  } catch { /* keep default Misc option */ }
}

// ── browse ──
async function loadPackages () {
  const r = await fetch( '/api/packages' );
  if ( !r.ok ) { showStatus( 'Failed to load packages', false ); return; }
  PACKAGES = await r.json();
  renderList();
  // package-id datalist for the upload form
  const ids = $( '#pkg-ids' );
  ids.innerHTML = '';
  for ( const nm of Object.keys( PACKAGES.packages || {} ).sort() ) {
    const o = document.createElement( 'option' ); o.value = nm; ids.appendChild( o );
  }
  refreshUploadMeta();
}
function renderList () {
  const q = ( $( '#pkg-search' ).value || '' ).toLowerCase();
  const list = $( '#pkg-list' );
  list.innerHTML = '';
  const names = Object.keys( PACKAGES.packages || {} ).sort();
  let count = 0;
  for ( const nm of names ) {
    if ( q && !nm.toLowerCase().includes( q ) ) continue;
    const pkg = PACKAGES.packages[ nm ];
    const vers = Object.keys( pkg.versions || {} ).sort();
    const v = pkg.versions[ vers[ vers.length - 1 ] ];
    const cat = v && v.category ? v.category : 'Misc';
    const div = document.createElement( 'div' );
    div.className = 'pkg-item' + ( nm === currentPkg ? ' active' : '' );
    div.innerHTML = `<div class="nm"><button class="ghost add-ver" title="Add a new version" type="button">+ ver</button> ${ esc( nm ) }</div>
      <div class="meta"><span class="cat">${ esc( cat ) }</span> · ${ vers.length } version${ vers.length > 1 ? 's' : '' } · latest ${ esc( vers[ vers.length - 1 ] || '' ) }</div>`;
    div.onclick = () => selectPackage( nm );
    div.querySelector( '.add-ver' ).onclick = ( e ) => { e.stopPropagation(); openUploadModal( nm ); };
    list.appendChild( div );
    count++;
  }
  if ( !count ) list.innerHTML = '<div class="empty">No packages match.</div>';
}
async function selectPackage ( nm ) {
  currentPkg = nm;
  currentVer = null;
  renderList();
  const pkg = PACKAGES.packages[ nm ];
  const vers = Object.keys( pkg.versions || {} ).sort();
  if ( !vers.length ) { $( '#pkg-detail' ).innerHTML = '<div class="empty">No versions.</div>'; return; }
  selectVersion( vers[ vers.length - 1 ] );
}
async function selectVersion ( ver ) {
  currentVer = ver;
  const pkg = PACKAGES.packages[ currentPkg ];
  const vers = Object.keys( pkg.versions || {} ).sort();
  const v = pkg.versions[ ver ] || {};
  const cat = v.category || 'Misc';
  const d = $( '#pkg-detail' );
  const rows = vers.map( x => `
    <div class="ver-row ${ x === ver ? 'active' : '' }" data-ver="${ esc( x ) }">
      <button class="ver-row-btn" type="button">
        <span class="caret">${ x === ver ? '▼' : '▶' }</span><span class="ver-nm">${ esc( x ) }</span>
      </button>
      ${ x === ver ? `<div class="ver-body">
        <div id="deps-panel"></div>
        <div id="file-list"><div class="empty">Loading files…</div></div>
        <div id="viewer" class="hidden"><div class="fhead" id="viewer-head"></div><pre id="viewer-body"></pre></div>
      </div>` : '' }
    </div>`).join( '' );
  d.innerHTML = `
    <div class="pkg-detail-head">
      <button class="ghost" id="btn-addver">+ Add version</button>
      <h3>${ esc( currentPkg ) }</h3>
      <span class="badge">${ esc( cat ) }</span>
      <button class="danger" id="btn-del" style="margin-left:auto">Delete v${ esc( ver ) }</button>
    </div>
    <div class="pkg-json" id="pkg-json-block">
      <div class="pkg-json-head"><span class="pkg-json-caret">▾</span>package.json</div>
      <pre class="pkg-json-content" id="pkg-json-content">loading…</pre>
    </div>
    <div class="version-list">${ rows }</div>`;
  d.querySelectorAll( '.ver-row-btn' ).forEach( btn => {
    btn.onclick = () => selectVersion( btn.closest( '.ver-row' ).dataset.ver );
  } );
  $( '#pkg-json-block .pkg-json-head' ).onclick = () => {
    const body = $( '#pkg-json-content' );
    const hidden = body.classList.toggle( 'hidden' );
    $( '#pkg-json-block .pkg-json-caret' ).textContent = hidden ? '▸' : '▾';
  };
  $( '#btn-del' ).onclick = async () => {
    if ( !confirm( `Delete ${ currentPkg } v${ ver }? This removes the version from the registry.` ) ) return;
    const r = await fetch( `/api/delete/${ encodeURIComponent( currentPkg ) }/${ encodeURIComponent( ver ) }` );
    const j = await r.json();
    if ( j.ok ) { showStatus( j.message, true ); await loadPackages(); selectPackage( currentPkg ); }
    else showStatus( j.error || 'delete failed', false );
  };
  $( '#btn-addver' ).onclick = () => openUploadModal( currentPkg );
  fetch( `/api/package/${ encodeURIComponent( currentPkg ) }/${ encodeURIComponent( ver ) }/json` )
    .then( r => r.json() )
    .then( j => { $( '#pkg-json-content' ).textContent = JSON.stringify( j, null, 2 ); } )
    .catch( () => { $( '#pkg-json-content' ).textContent = 'failed to load package.json'; } );
  const r = await fetch( `/api/package/${ encodeURIComponent( currentPkg ) }/${ encodeURIComponent( ver ) }/files` );
  const j = await r.json();
  if ( !r.ok || !j.files ) { $( '#file-list' ).innerHTML = `<div class="empty">${ esc( j.error || 'error' ) }</div>`; return; }
  renderFileTree( j.files );
  loadDeps( ver );
}

// ── xplore-style folder view ──
function renderFileTree ( files ) {
  const container = $( '#file-list' );
  container.innerHTML = '';
  const tree = {};
  for ( const f of files ) {
    const parts = f.name.split( '/' ).filter( Boolean );
    let node = tree;
    for ( let i = 0; i < parts.length - 1; i++ ) {
      node[ parts[ i ] ] = node[ parts[ i ] ] || {};
      node = node[ parts[ i ] ];
    }
    node[ parts[ parts.length - 1 ] ] = { file: f };
  }
  const rootEl = document.createElement( 'div' );
  rootEl.className = 'files tree';
  let build = ( obj, depth, parent, rel_path ) => {
    for ( const k of Object.keys( obj ).sort() ) {
      const v = obj[ k ];
      const row = document.createElement( 'div' );
      row.style.paddingLeft = ( depth * 16 + 8 ) + 'px';
      if ( v && v.file ) {
        const f = v.file;
        row.className = 'frow';
        row.dataset.path = f.name;
        const left = document.createElement( 'span' );
        left.textContent = f.name.split( '/' ).pop();
        const size = document.createElement( 'span' );
        size.className = 'fsz'; size.textContent = fmtSize( f.size );
        row.append( left, size );
        row.onclick = ( e ) => { e.stopPropagation(); viewFile( f.name, f.size ); };
        parent.appendChild( row );
      } else {
        const folderPath = rel_path ? rel_path + '/' + k : k;
        row.className = 'fdir';
        row.dataset.path = folderPath;
        row.innerHTML = `<span class="caret">▶</span><span>📁 ${ esc( k ) }</span><button class="convert-dep" title="Convert folder to a VPM dependency and remove it from the package" style="margin-left:4px; padding:0 4px; font-size:10px">+<span class="muted" style="font-size:8px">dep</span></button>`;
        const sub = document.createElement( 'div' );
        sub.className = 'tree-sub hidden';
        build( v, depth + 1, sub, folderPath );
        row.appendChild( sub );
        row.onclick = ( e ) => {
          e.stopPropagation();
          const hidden = sub.classList.toggle( 'hidden' );
          row.querySelector( '.caret' ).textContent = hidden ? '▶' : '▼';
        };
        row.querySelector( '.convert-dep' ).onclick = async ( e ) => {
          e.stopPropagation();
          const full = row.dataset.path;
          const folderName = full.split( '/' ).pop();
          if ( !confirm( `Convert folder '${ folderName }' to a VPM dependency and remove it from ${ currentPkg } v${ currentVer }?` ) ) return;
          const rr = await fetch( `/api/package/${ encodeURIComponent( currentPkg ) }/${ encodeURIComponent( currentVer ) }/convert-to-dep?path=${ encodeURIComponent( full ) }` );
          const jj = await rr.json();
          if ( jj.ok ) { showStatus( jj.message, true ); selectVersion( currentVer ); }
          else showStatus( jj.error || 'convert failed', false );
        };
        parent.appendChild( row );
      }
    }
  };
  build( tree, 0, rootEl, "" );
  container.appendChild( rootEl );
}
async function viewFile ( path, size ) {
  const fl = $( '#file-list' );
  fl.querySelectorAll( '.frow' ).forEach( x => x.classList.remove( 'selected' ) );
  fl.querySelectorAll( '.frow' ).forEach( row => {
    if ( row.dataset.path === path ) row.classList.add( 'selected' );
  } );
  const rr = await fetch( `/api/package/${ encodeURIComponent( currentPkg ) }/${ encodeURIComponent( currentVer ) }/file?path=${ encodeURIComponent( path ) }` );
  const jj = await rr.json();
  $( '#viewer' ).classList.remove( 'hidden' );
  $( '#viewer-head' ).textContent = `${ path } · ${ fmtSize( jj.size ) }`;
  $( '#viewer-body' ).textContent = jj.content;
}

// ── dependencies editor ──
async function loadDeps ( ver ) {
  const panel = $( '#deps-panel' );
  const r = await fetch( `/api/package/${ encodeURIComponent( currentPkg ) }/${ encodeURIComponent( ver ) }/deps` );
  const j = await r.json();
  if ( !r.ok ) { panel.innerHTML = `<div class="empty">${ esc( j.error || 'error' ) }</div>`; return; }
  renderDeps( ver, j.dependencies || {}, j.vpmDependencies || {} );
}
function depsRows ( obj ) {
  return Object.entries( obj || {} ).map( ( [ k, v ] ) => ( { k, v: String( v ) } ) );
}
function renderDeps ( ver, deps, vpmDeps ) {
  const panel = $( '#deps-panel' );
  const rows = [
    ...depsRows( deps ).map( r => ( { sec: 'dependencies', k: r.k, v: r.v } ) ),
    ...depsRows( vpmDeps ).map( r => ( { sec: 'vpmDependencies', k: r.k, v: r.v } ) ),
  ];
  panel.innerHTML = `<div class="card deps-card">
    <div class="deps-head" id="deps-head" title="toggle">
      <button class="ghost deps-toggle" id="deps-toggle">▶</button>
      <span class="deps-title">Dependencies</span>
      <span class="deps-count">${ rows.length }</span>
      <span class="muted" style="font-size:11px">deps · vpmDeps</span>
      <button class="ghost" id="deps-save" style="margin-left:auto">💾 Save</button>
    </div>
    <div id="deps-rows" class="hidden">
      <div class="dep-head-row"><span>type</span><span>package</span><span></span><span>version</span><span></span></div>
      ${ rows.length ? rows.map( r => rowHtml( r.sec, r.k, r.v ) ).join( '' )
      : `<div class="empty" style="padding:6px">No dependencies.</div>` }
      <div class="grid" style="grid-template-columns:1fr 1fr; gap:12px">
      <button class="ghost add-row" style="width:100%">+ add by ID</button>
      <button class="ghost add-row-dir" style="width:100%">+ add from dir</button>
    </div>
    <input type="file" id="deps-dir-input" style="display:none" webkitdirectory>
    </div>
  </div>`;
  const toggle = () => {
    const box = $( '#deps-rows' );
    const hidden = box.classList.toggle( 'hidden' );
    $( '#deps-toggle' ).textContent = hidden ? '▶' : '▼';
  };
  $( '#deps-head' ).onclick = toggle;
  $( '#deps-toggle' ).onclick = ( e ) => { e.stopPropagation(); toggle(); };
  panel.querySelector( '.add-row' ).onclick = () => {
    const box = $( '#deps-rows' );
    const empty = box.querySelector( '.empty' );
    if ( empty ) empty.remove();
    box.insertAdjacentHTML( 'beforeend', rowHtml( 'dependencies', '', '' ) );
    box.querySelector( '.dep-row:last-child .dep-k' ).focus();
  };
  panel.querySelector( '.add-row-dir' ).onclick = ( e ) => {
    e.stopPropagation();
    $( '#deps-dir-input' ).click();
  };
  $( '#deps-dir-input' ).onchange = ( e ) => {
    const files = e.target.files || [];
    if ( !files.length ) return;
    // use the folder name (first path segment) as the dependency id
    const folder = files[ 0 ].webkitRelativePath.split( '/' )[ 0 ];
    const box = $( '#deps-rows' );
    const empty = box.querySelector( '.empty' );
    if ( empty ) empty.remove();
    box.insertAdjacentHTML( 'beforeend', rowHtml( 'vpmDependencies', folder || '', '1.0.0' ) );
    e.target.value = '';
  };
  $( '#deps-save' ).onclick = async ( e ) => {
    e.stopPropagation();
    const out = { dependencies: {}, vpmDependencies: {} };
    $( '#deps-rows' ).querySelectorAll( '.dep-row' ).forEach( row => {
      const sec = row.querySelector( '.dep-sec' ).value;
      const k = row.querySelector( '.dep-k' ).value.trim();
      const v = row.querySelector( '.dep-v' ).value.trim();
      if ( k ) out[ sec ][ k ] = v;
    } );
    const payload = JSON.stringify( out );
    const r = await fetch( `/api/package/${ encodeURIComponent( currentPkg ) }/${ encodeURIComponent( ver ) }/deps`, {
      method: 'POST', headers: { 'Content-Type': 'application/json' }, body: payload,
    } );
    const j = await r.json();
    showStatus( j.ok ? j.message : ( j.error || 'save failed' ), j.ok );
    if ( j.ok ) { await loadDeps( ver ); }
  };
}
function rowHtml ( sec, k, v ) {
  return `<div class="dep-row">
    <select class="dep-sec">${ sec === 'vpmDependencies'
      ? '<option value="dependencies">dependencies</option><option value="vpmDependencies" selected>vpmDependencies</option>'
      : '<option value="dependencies" selected>dependencies</option><option value="vpmDependencies">vpmDependencies</option>' }</select>
    <input class="dep-k" value="${ esc( k ) }" placeholder="com.package.id" spellcheck="false" title="Repo: ${ esc( k ) }">
    <span class="dep-arrow">→</span>
    <input class="dep-v" value="${ esc( v ) }" placeholder="1.0.0 or ^1.2.3">
    <button class="ghost dep-del" title="remove">✕</button>
  </div>`;
}
document.addEventListener( 'click', ( e ) => {
  if ( e.target.classList && e.target.classList.contains( 'dep-del' ) ) {
    e.target.closest( '.dep-row' ).remove();
  }
} );

// ── upload: existing id/version helpers ──
function pkgInfo () {
  const nm = $( '#up-name' ).value.trim();
  return ( PACKAGES.packages || {} )[ nm ];
}
function latestVersion ( info ) {
  const vs = info ? Object.keys( info.versions || {} ) : [];
  return vs.length ? vs.slice().sort().at( -1 ) : null;
}
function refreshUploadMeta () {
  const info = pkgInfo();
  // autofill category from existing package
  const infoCat = info && Object.values( info.versions || {} ).length
    ? Object.values( info.versions )[ 0 ].category : null;
  if ( infoCat && $( '#up-category' ).value !== infoCat ) $( '#up-category' ).value = infoCat;
  // version datalist for this package
  const vl = $( '#pkg-versions' );
  vl.innerHTML = '';
  if ( info ) {
    for ( const v of Object.keys( info.versions || {} ).sort() ) {
      const o = document.createElement( 'option' ); o.value = v; vl.appendChild( o );
    }
  }
  checkVersionConflict();
}
function checkVersionConflict () {
  const nm = $( '#up-name' ).value.trim();
  const ver = $( '#up-version' ).value.trim();
  const warn = $( '#up-warn' );
  const info = pkgInfo();
  const existing = info && ver ? info.versions[ ver ] : null;
  if ( !info || !existing ) { warn.classList.add( 'hidden' ); warn.innerHTML = ''; return; }
  const latest = latestVersion( info );
  const prefix = ver + '-rc.';
  const rcs = Object.keys( info.versions || {} ).filter( v => v.startsWith( prefix ) ).sort();
  let slots = '';
  if ( rcs.length ) {
    slots = `<label style="margin-top:8px">Insert into slot
      <select name="rc_slot" id="rc-slot" style="width:auto">
        <option value="">next free slot</option>`;
    for ( const r of rcs ) {
      const n = r.slice( prefix.length );
      slots += `<option value="${ n }">rc.${ n } (currently ${ r })</option>`;
    }
    slots += `</select></label>`;
  }
  const autoRc = `${ ver }-rc.${ rcs.length + 1 }`;
  warn.innerHTML =
    `<div>⚠ <b>Version ${ ver } already exists</b>` +
    ( latest && latest !== ver ? ` — latest version is <b>${ latest }</b>.` : '.' ) + `</div>` +
    `<label style="display:flex;align-items:center;gap:8px;margin-top:8px">
      <input type="checkbox" name="demote" id="up-demote" value="1" style="width:auto">
      Demote the existing <b>${ ver }</b> to an RC candidate, so this upload becomes the new <b>${ ver }</b>
    </label>` +
    `<div id="rc-slot-wrap" class="hidden" style="margin-left:26px">` +
    ( slots || '<div class="muted" style="margin-top:6px">No existing rc candidates — the old version becomes <code>rc.1</code>.</div>' ) +
    `<div class="muted" style="margin-top:4px">Existing rc candidates at/after the chosen slot are upgraded by 1.</div></div>` +
    `<div class="muted" style="margin-top:8px">If left unchecked, your upload is published as <code>${ autoRc }</code> and the existing <b>${ ver }</b> is kept intact.</div>`;
  warn.classList.remove( 'hidden' );
  $( '#up-demote' ).addEventListener( 'change', () => {
    $( '#rc-slot-wrap' ).classList.toggle( 'hidden', !$( '#up-demote' ).checked );
  } );
}
$( '#up-name' ).addEventListener( 'input', refreshUploadMeta );
$( '#up-version' ).addEventListener( 'input', checkVersionConflict );

// ── upload ──
$( '#upload-form' ).addEventListener( 'submit', async ( e ) => {
  e.preventDefault();
  const fd = new FormData( e.target );
  const btn = e.target.querySelector( 'button' );
  btn.disabled = true; btn.textContent = 'Uploading…';
  showStatus( 'Uploading and publishing…', true );
  try {
    const r = await fetch( '/upload', { method: 'POST', body: fd } );
    const html = await r.text();
    // response is an HTML page; extract status + body text
    const m = html.match( /<h2[^>]*>(.*?)<\/h2>/ );
    const body = html.replace( /<[^>]+>/g, ' ' ).replace( /&amp;/g, '&' ).replace( /&lt;/g, '<' ).replace( /&gt;/g, '>' ).replace( /&quot;/g, '"' ).replace( /&#x27;/g, "'" ).replace( /<br>/g, '\n' ).trim();
    const ok = ( m && m[ 1 ].includes( 'successful' ) ) || html.includes( 'Upload successful' );
    showStatus( body, ok );
    if ( ok ) { loadPackages(); closeUploadModal(); }
  } catch ( err ) {
    showStatus( 'Network error: ' + err, false );
  }
  btn.disabled = false; btn.textContent = '⬆ Upload & publish';
} );

$( '#pkg-search' ).addEventListener( 'input', renderList );

loadCategories();
fetch( '/api/config' ).then( r => r.json() ).then( j => { CONFIG = j || CONFIG; } ).catch( () => { } );
loadPackages();

function showRegistry () {
  if ( CONFIG.master_repo_url ) window.open( CONFIG.master_repo_url, '_blank' );
}
function openVault () {
  if ( CONFIG.vault_url ) window.open( CONFIG.vault_url, '_blank' );
}

// ── repo metadata editor (repos.conf) ──
let REPOS = null;
async function openRepos () {
  const r = await fetch( '/api/repos' );
  const j = await r.json();
  const msg = $( '#repos-msg' );
  if ( !r.ok ) {
    msg.classList.remove( 'hidden' );
    msg.textContent = j.error || 'failed to load repo metadata';
    return;
  }
  msg.classList.add( 'hidden' );
  REPOS = j;
  $( '#repos-master-name' ).value = j.master_name || '';
  $( '#repos-master-id' ).value = j.master_id || '';
  $( '#repos-rows' ).innerHTML = ( j.categories || [] ).map( ( c, i ) => `
    <div class="dep-row" style="grid-template-columns:1fr 1.4fr 1.4fr">
      <input class="rep-cat" value="${ esc( c.name ) }" readonly title="category name (fixed)">
      <input class="rep-name" value="${ esc( c.repo_name || '' ) }" data-i="${ i }" autocomplete="off" spellcheck="false" placeholder="repo name">
      <input class="rep-id" value="${ esc( c.repo_id || '' ) }" data-i="${ i }" autocomplete="off" spellcheck="false" placeholder="com.author.repo">
    </div>`).join( '' );
}
function saveRepos () {
  const cats = ( REPOS.categories || [] ).map( ( c, i ) => {
    const nameEls = document.querySelectorAll( '.rep-name' );
    const idEls = document.querySelectorAll( '.rep-id' );
    return {
      name: c.name,
      repo_name: ( nameEls[ i ] ? nameEls[ i ].value : c.repo_name || '' ).trim(),
      repo_id: ( idEls[ i ] ? idEls[ i ].value : c.repo_id || '' ).trim(),
    };
  } );
  const payload = {
    master_name: $( '#repos-master-name' ).value.trim(),
    master_id: $( '#repos-master-id' ).value.trim(),
    categories: cats,
  };
  const hint = $( '#repos-saved-hint' );
  hint.textContent = 'saving…';
  $( '#repos-msg' ).classList.add( 'hidden' );
  fetch( '/api/repos', {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify( payload ),
  } ).then( async r => {
    const j = await r.json();
    if ( j.ok ) {
      hint.textContent = 'saved ✓';
      await openRepos();
      setTimeout( () => hint.textContent = '', 2500 );
    } else {
      hint.textContent = j.error || 'save failed';
      $( '#repos-msg' ).classList.remove( 'hidden' );
      $( '#repos-msg' ).textContent = ( j.error || 'save failed' ) + ' (category repos were not regenerated)';
    }
  } ).catch( () => {
    hint.textContent = 'error';
    $( '#repos-msg' ).classList.remove( 'hidden' );
    $( '#repos-msg' ).textContent = 'Network error saving repo metadata';
  } );
}
$( '#repos-save' ).onclick = saveRepos;
