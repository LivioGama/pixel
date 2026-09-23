import {AbsoluteFill, useCurrentFrame} from 'remotion';
import {Badge, halo, LoopArrow, Node, NodeSpec, Panel, Rail, TravelDot} from './parts';

const RED = '#d97a6b';
const GREEN = '#3fb950';
const DIM = '#8b949e';
const BLUE = '#58a6ff';

const topY = 150;
const topXs = [140, 360, 580, 800, 1020, 1240, 1460];
const topNodes: NodeSpec[] = [
	{x: topXs[0], y: topY, label: 'Task', sub: 'new session', icon: 'task'},
	{x: topXs[1], y: topY, label: 'Agent', sub: 'zero memory', icon: 'agent'},
	{x: topXs[2], y: topY, label: 'Blind Reads', sub: 'rg · cat · sed', icon: 'search'},
	{x: topXs[3], y: topY, label: 'Dead Ends', sub: 'wrong files', icon: 'deadend', warn: true},
	{x: topXs[4], y: topY, label: 'Guess Impact', sub: 're-derive', icon: 'guess'},
	{x: topXs[5], y: topY, label: 'Task Shipped', sub: 'budget spent', icon: 'ship'},
	{x: topXs[6], y: topY, label: 'Savings', sub: 'claimed, unmeasured', icon: 'lost'},
];

const setupY = 520;
const setupXs = [600, 830, 1060, 1290];
const setupNodes: NodeSpec[] = [
	{x: setupXs[0], y: setupY, label: 'Repo', sub: 'any language', icon: 'repo'},
	{x: setupXs[1], y: setupY, label: 'prepare-repo', sub: 'index + graph', icon: 'init'},
	{x: setupXs[2], y: setupY, label: '.pixel/', sub: 'shards · call graph', icon: 'graph'},
	{x: setupXs[3], y: setupY, label: 'In Repo', sub: 'git-anchored', icon: 'git'},
];

const sessY = 780;
const sessXs = [1440, 1200, 960, 720, 480, 240];
const sessNodes: NodeSpec[] = [
	{x: sessXs[0], y: sessY, label: 'Session Hook', sub: 'use pixel first', icon: 'hook', accent: GREEN},
	{x: sessXs[1], y: sessY, label: 'Task', sub: 'new session', icon: 'task'},
	{x: sessXs[2], y: sessY, label: 'Agent', sub: 'asks pixel', icon: 'agent'},
	{x: sessXs[3], y: sessY, label: 'find-code · impact', sub: 'bounded reads', icon: 'read', accent: GREEN},
	{x: sessXs[4], y: sessY, label: 'Precise Context', sub: 'no guessing', icon: 'context'},
	{x: sessXs[5], y: sessY, label: 'token-savings', sub: 'measured', icon: 'meter', accent: GREEN},
];

const arrivals = (xs: number[], dur: number) => xs.map((_, i) => (i * dur) / (xs.length - 1));

const topDur = 360;
const setupDur = 240;
const sessDur = 360;

const topArrivals = arrivals(topXs, topDur);
const setupArrivals = arrivals(setupXs, setupDur);
const sessArrivals = arrivals(sessXs, sessDur);

export const PixelComparison = () => {
	const frame = useCurrentFrame();
	return (
		<AbsoluteFill style={{background: '#0d1117'}}>
			<svg width={1600} height={1000} viewBox="0 0 1600 1000">
				{/* ── WITHOUT PIXEL ─────────────────────────── */}
				<Panel x={40} y={46} w={1520} h={300} title="WITHOUT PIXEL" color="#4a3a36"/>
				<Rail nodes={topNodes} color="#6b4a42"/>
				<g transform={`translate(0, ${topY})`}>
					<TravelDot frame={frame} xs={topXs} duration={topDur} color={RED}/>
				</g>
				{topNodes.map((n, i) => (
					<Node key={n.label} spec={n} glow={halo(frame, topArrivals[i])}/>
				))}
				<LoopArrow x1={140} x2={1460} y={330} color={RED} label="REPEATS EVERY SESSION · EVERY TEAMMATE'S AGENT"/>

				{/* ── WITH PIXEL ────────────────────────────── */}
				<Panel x={40} y={400} w={1520} h={560} title="WITH PIXEL" color={BLUE}/>

				<text x={120} y={540} fontFamily="Inter,Arial,sans-serif" fontSize={34} fontWeight={800} fill="#3d444d" letterSpacing={1}>
					<tspan x={120} dy={0}>INDEX ALREADY</tspan>
					<tspan x={120} dy={40}>KNOWS YOUR</tspan>
					<tspan x={120} dy={40}>REPO</tspan>
				</text>

				{/* setup rail → connector down to session hook */}
				<Rail nodes={setupNodes} color="#2f4a75"/>
				<path d={`M ${setupXs[3] + 42} ${setupY} H 1490 V ${sessY - 42}`} fill="none" stroke="#2f4a75" strokeWidth={2.6} opacity={0.7}/>
				<Badge x={1420} y={640} text="FRESH EVERY HEAD" color={DIM}/>

				<g transform={`translate(0, ${setupY})`}>
					{frame <= setupDur ? <TravelDot frame={frame} xs={setupXs} duration={setupDur} color={GREEN}/> : null}
				</g>
				{setupNodes.map((n, i) => (
					<Node key={n.label} spec={n} glow={halo(frame, setupArrivals[i])}/>
				))}

				{/* session rail, right → left */}
				<Rail nodes={sessNodes} color="#2f4a75"/>
				<g transform={`translate(0, ${sessY})`}>
					<TravelDot frame={frame} xs={sessXs} duration={sessDur} color={GREEN}/>
				</g>
				{sessNodes.map((n, i) => (
					<Node key={n.label} spec={n} glow={halo(frame, sessArrivals[i])}/>
				))}

				<LoopArrow x1={240} x2={1440} y={930} color={BLUE} label="ONE .PIXEL DIR · EVERY AGENT · SAVINGS MEASURED, NOT CLAIMED"/>
			</svg>
		</AbsoluteFill>
	);
};
