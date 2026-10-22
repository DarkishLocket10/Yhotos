function E(t){let e,n=()=>{e?.abort(),e=new AbortController,t(e.signal)};document.addEventListener("yhotos:navigate",n),n()}function O(t){if(t.length<5||t.length>64)throw new Error("Invalid placeholder");let e=t[0]|t[1]<<8|t[2]<<16,n=t[3]|t[4]<<8,d=!!(e&8388608),r=!!(n&32768),l=n&7,a=d?5:7;if(!l||l>a)throw new Error("Invalid placeholder");let s=r?a:l,h=r?l:a,f=s/h,v=Math.round(f>1?32:32*f),x=Math.round(f>1?32/f:32),M=0,C=d?6:5,p=(c,i,b,g)=>{let u=[];for(let o=0;o<i;o++)for(let m=o?0:1;m*i<c*(i-o);m++){let y=C+(M>>1);if(y>=t.length)throw new Error("Invalid placeholder");let w=((t[y]>>(M++&1)*4&15)/7.5-1)*g;u.push([m,o,w])}return(o,m)=>u.reduce((y,[w,q,H])=>y+H*o[w]*m[q]*2,b)},D=p(Math.max(3,s),Math.max(3,h),(e&63)/63,(e>>18&31)/31),L=p(3,3,(e>>6&63)/31.5-1,(n>>3&63)/63*1.25),U=p(3,3,(e>>12&63)/31.5-1,(n>>9&63)/63*1.25),K=d?p(5,5,(t[5]&15)/15,(t[5]>>4)/15):()=>1,A=c=>Array.from({length:c},(i,b)=>Array.from({length:7},(g,u)=>Math.cos(Math.PI/c*(b+.5)*u))),T=A(v),k=A(x),I=new Uint8ClampedArray(v*x*4),S=0;for(let c of k)for(let i of T){let b=D(i,c),g=b-2/3*L(i,c),u=U(i,c),o=(3*b-g+u)/2;for(let m of[o,o-u,g,K(i,c)])I[S++]=Math.floor(Math.max(0,Math.min(1,m))*255)}return{w:v,h:x,rgba:I}}typeof document<"u"&&"IntersectionObserver"in globalThis&&E(t=>{let e=new IntersectionObserver(n=>{for(let d of n){if(!d.isIntersecting)continue;let r=d.target;if(e.unobserve(r),!(r.complete&&r.naturalWidth>0))try{let l=r.dataset.thumbhash;if(!l||l.length>88)continue;let a=O(Uint8Array.from(atob(l),f=>f.charCodeAt(0))),s=document.createElement("canvas");s.width=a.w,s.height=a.h;let h=s.getContext("2d");if(!h)continue;h.putImageData(new ImageData(a.rgba,a.w,a.h),0,0),r.style.backgroundImage=`url(${s.toDataURL("image/png")})`,r.style.backgroundSize="cover"}catch{}}},{rootMargin:"200px"});document.querySelectorAll('img[loading="lazy"][data-thumbhash]').forEach(n=>e.observe(n)),t.addEventListener("abort",()=>e.disconnect())});export{O as decodeThumbhash};
/*!
 * ThumbHash decoder adapted from https://github.com/evanw/thumbhash (MIT).
 * Copyright (c) 2023 Evan Wallace. Bounded input checks and lifecycle added here.
 * Permission is hereby granted, free of charge, to any person obtaining a copy
 * of this software and associated documentation files (the "Software"), to deal
 * in the Software without restriction, including without limitation the rights
 * to use, copy, modify, merge, publish, distribute, sublicense, and/or sell copies
 * of the Software, and to permit persons to whom the Software is furnished to do
 * so, subject to the following conditions:
 * The above copyright notice and this permission notice shall be included in all
 * copies or substantial portions of the Software.
 * THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
 * IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
 * FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
 * AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
 * LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
 * OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
 * SOFTWARE.
 */
