// Invoked by the populated admin HTTP integration test. Session material is supplied
// over stdin and remains only in memory; the only artifacts are page screenshots.
import {mkdir} from 'node:fs/promises';
const chunks=[];
for await (const chunk of process.stdin) chunks.push(chunk);
const {base,session,pages}=JSON.parse(Buffer.concat(chunks).toString());
const {chromium}=await import(process.env.YHOTOS_PLAYWRIGHT_MODULE || 'playwright');
const browser=await chromium.launch({headless:true,channel:process.env.YHOTOS_BROWSER_CHANNEL || 'chrome'});
try {
  const context=await browser.newContext({viewport:{width:1440,height:1000},colorScheme:'light'});
  await context.addCookies([{name:'ya',value:session,url:base,sameSite:'Strict'}]);
  const page=await context.newPage();
  await mkdir('deploy/dev/admin-walkthrough',{recursive:true});
  for (const [name,path] of pages){
    const response=await page.goto(base+path,{waitUntil:'networkidle'});
    if (response?.status()!==200 || page.url()!==base+path) throw new Error('Admin page navigation failed');
    await page.screenshot({path:`deploy/dev/admin-walkthrough/${name}.png`,fullPage:true});
  }
  await context.close();
} finally {await browser.close();}
