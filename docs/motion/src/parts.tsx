import {interpolate, useCurrentFrame} from 'remotion';

export type NodeSpec = {
	x: number;
	y: number;
	label: string;
	sub: string;
	icon: IconKind;
	accent?: string;
	warn?: boolean;
};

export type IconKind =
	| 'task'
	| 'agent'
	| 'search'
	| 'deadend'
	| 'guess'
	| 'ship'
	| 'lost'
	| 'repo'
	| 'init'
	| 'graph'
	| 'git'
	| 'hook'
	| 'read'
	| 'context'
	| 'meter'
	| 'edit'
	| 'break'
	| 'shield';

const TILE = 84;

export const Icon = ({kind, color}: {kind: IconKind; color: string}) => {
	const s = {stroke: color, strokeWidth: 3.4, fill: 'none', strokeLinecap: 'round' as const, strokeLinejoin: 'round' as const};
	switch (kind) {
		case 'task':
			return (<g {...s}><rect x="14" y="18" width="36" height="28" rx="6"/><line x1="21" y1="27" x2="43" y2="27"/><line x1="21" y1="35" x2="36" y2="35"/></g>);
		case 'agent':
			return (<g {...s}><rect x="14" y="18" width="36" height="28" rx="9" fill={color} stroke="none"/><ellipse cx="26" cy="32" rx="4" ry="5.5" fill="#0d1117"/><ellipse cx="38" cy="32" rx="4" ry="5.5" fill="#0d1117"/></g>);
		case 'search':
			return (<g {...s}><rect x="14" y="16" width="15" height="15" rx="3"/><rect x="35" y="16" width="15" height="15" rx="3"/><rect x="14" y="37" width="15" height="15" rx="3"/><circle cx="42" cy="42" r="9"/><line x1="48" y1="48" x2="55" y2="55"/></g>);
		case 'deadend':
			return (<g {...s}><rect x="15" y="16" width="22" height="30" rx="4"/><rect x="29" y="16" width="22" height="30" rx="4" transform="rotate(6 40 31)"/><line x1="22" y1="24" x2="32" y2="24"/></g>);
		case 'guess':
			return (<g {...s}><circle cx="22" cy="22" r="5"/><circle cx="44" cy="30" r="5"/><circle cx="26" cy="46" r="5"/><line x1="26" y1="25" x2="40" y2="28"/><line x1="41" y1="35" x2="30" y2="42"/><line x1="52" y1="14" x2="58" y2="8" strokeDasharray="1 6"/></g>);
		case 'ship':
			return (<g {...s}><rect x="16" y="18" width="30" height="30" rx="6"/><path d="M23 33 l6 6 l12 -13"/></g>);
		case 'lost':
			return (<g {...s}><rect x="16" y="14" width="32" height="38" rx="5"/><line x1="22" y1="22" x2="42" y2="22"/><line x1="22" y1="30" x2="42" y2="30"/><line x1="22" y1="38" x2="34" y2="38"/><circle cx="52" cy="16" r="2" stroke="none" fill={color}/><circle cx="58" cy="26" r="2" stroke="none" fill={color}/><circle cx="52" cy="36" r="2" stroke="none" fill={color}/></g>);
		case 'repo':
			return (<g {...s}><path d="M16 20 h14 l4 5 h14 v22 h-32 z"/><line x1="22" y1="34" x2="42" y2="34"/><line x1="22" y1="40" x2="36" y2="40"/></g>);
		case 'init':
			return (<g {...s}><circle cx="24" cy="22" r="5"/><circle cx="44" cy="30" r="5"/><circle cx="26" cy="46" r="5"/><line x1="28" y1="25" x2="40" y2="28"/><line x1="41" y1="35" x2="30" y2="43"/></g>);
		case 'graph':
			return (<g {...s}><rect x="14" y="18" width="36" height="28" rx="6"/><rect x="20" y="36" width="24" height="4" rx="2" fill={color} stroke="none"/><rect x="20" y="28" width="14" height="4" rx="2" fill={color} stroke="none"/></g>);
		case 'git':
			return (<g {...s}><line x1="32" y1="12" x2="32" y2="52"/><circle cx="32" cy="18" r="4"/><circle cx="32" cy="46" r="4"/><path d="M32 30 q12 2 16 -8"/><circle cx="48" cy="20" r="4"/></g>);
		case 'hook':
			return (<g {...s}><path d="M40 12 v18 a10 10 0 1 1 -16 0 v-4"/><path d="M24 22 l6 6"/><path d="M40 12 l5 5 M40 12 l-5 5" transform="translate(0 0)"/></g>);
		case 'read':
			return (<g {...s}><rect x="16" y="14" width="32" height="38" rx="5"/><line x1="22" y1="23" x2="42" y2="23"/><line x1="22" y1="31" x2="42" y2="31"/><line x1="22" y1="39" x2="36" y2="39"/><line x1="22" y1="47" x2="30" y2="47"/></g>);
		case 'context':
			return (<g {...s}><rect x="18" y="16" width="28" height="34" rx="5"/><rect x="23" y="22" width="18" height="6" rx="2" fill={color} stroke="none"/><line x1="23" y1="34" x2="41" y2="34"/><line x1="23" y1="40" x2="35" y2="40"/></g>);
		case 'meter':
			return (<g {...s}><path d="M18 46 a15 15 0 0 1 28 0"/><line x1="32" y1="42" x2="42" y2="28"/><circle cx="32" cy="44" r="3.5" fill={color} stroke="none"/></g>);
		case 'edit':
			return (<g {...s}><path d="M40 14 l10 10 -22 22 -12 2 2 -12 z"/><line x1="35" y1="19" x2="45" y2="29"/></g>);
		case 'break':
			return (<g {...s}><path d="M26 22 l-8 8 8 8"/><path d="M38 22 l8 8 -8 8"/><line x1="34" y1="16" x2="30" y2="46"/></g>);
		case 'shield':
			return (<g {...s}><path d="M32 12 l18 7 v12 c0 12 -8 19 -18 23 -10 -4 -18 -11 -18 -23 v-12 z"/><path d="M25 32 l5 5 10 -10"/></g>);
	}
};

export const Node = ({spec, glow}: {spec: NodeSpec; glow: number}) => {
	const accent = spec.accent ?? '#8b949e';
	const glowOpacity = interpolate(glow, [0, 1], [0, 0.55]);
	return (
		<g transform={`translate(${spec.x - TILE / 2}, ${spec.y - TILE / 2})`}>
			<circle cx={TILE / 2} cy={TILE / 2} r={TILE * 0.95} fill={accent} opacity={glowOpacity * 0.35}/>
			<circle cx={TILE / 2} cy={TILE / 2} r={TILE * 0.7} fill={accent} opacity={glowOpacity * 0.45}/>
			<rect
				width={TILE}
				height={TILE}
				rx={20}
				fill={spec.warn ? '#2a1210' : '#151a21'}
				stroke={spec.warn ? '#d97a6b' : accent}
				strokeWidth={spec.warn || spec.accent ? 2.6 : 1.6}
			/>
			<g transform="translate(10,10)">
				<Icon kind={spec.icon} color={spec.warn ? '#d97a6b' : accent}/>
			</g>
			<text x={TILE / 2} y={TILE + 34} textAnchor="middle" fontFamily="Inter,Arial,sans-serif" fontSize={22} fontWeight={700} fill={spec.warn ? '#e8a99d' : '#e6edf3'}>
				{spec.label}
			</text>
			<text x={TILE / 2} y={TILE + 58} textAnchor="middle" fontFamily="Inter,Arial,sans-serif" fontSize={15} fontWeight={500} letterSpacing={2} fill={spec.warn ? '#b97f74' : '#7d8590'}>
				{spec.sub.toUpperCase()}
			</text>
		</g>
	);
};

export const Rail = ({nodes, color}: {nodes: NodeSpec[]; color: string}) => {
	const y = nodes[0].y;
	const x1 = Math.min(...nodes.map((n) => n.x)) + TILE / 2;
	const x2 = Math.max(...nodes.map((n) => n.x)) - TILE / 2;
	return <line x1={x1} y1={y} x2={x2} y2={y} stroke={color} strokeWidth={2.6} opacity={0.7}/>;
};

const ease = (t: number) => t;

export const dotX = (frame: number, xs: number[], duration: number) => {
	const seg = duration / (xs.length - 1);
	const i = Math.min(Math.floor(frame / seg), xs.length - 2);
	const t = ease((frame - i * seg) / seg);
	return xs[i] + (xs[i + 1] - xs[i]) * t;
};

export const TravelDot = ({frame, xs, duration, color}: {frame: number; xs: number[]; duration: number; color: string}) => {
	const x = dotX(frame, xs, duration);
	return (
		<g>
			<circle cx={x} cy={0} r={16} fill={color} opacity={0.25}/>
			<circle cx={x} cy={0} r={9} fill={color}/>
		</g>
	);
};

export const halo = (frame: number, arrival: number, span = 40) => {
	const d = Math.abs(frame - arrival);
	return interpolate(d, [0, span], [1, 0], {extrapolateLeft: 'clamp', extrapolateRight: 'clamp'});
};

export const LoopArrow = ({x1, x2, y, color, label}: {x1: number; x2: number; y: number; color: string; label: string}) => (
	<g>
		<path d={`M ${x2} ${y - 60} V ${y} H ${x1} V ${y - 60}`} fill="none" stroke={color} strokeWidth={2.2} opacity={0.75}/>
		<path d={`M ${x1} ${y - 74} l -8 14 h 16 z`} fill={color} opacity={0.9}/>
		<rect x={(x1 + x2) / 2 - label.length * 5.6} y={y - 16} width={label.length * 11.2} height={32} rx={16} fill="#1b2028" stroke={color} strokeWidth={1.4}/>
		<text x={(x1 + x2) / 2} y={y + 6} textAnchor="middle" fontFamily="Inter,Arial,sans-serif" fontSize={17} fontWeight={600} letterSpacing={1.5} fill={color}>
			{label}
		</text>
	</g>
);

export const Panel = ({x, y, w, h, title, color}: {x: number; y: number; w: number; h: number; title: string; color: string}) => (
	<g>
		<rect x={x} y={y} width={w} height={h} rx={26} fill="none" stroke={color} strokeWidth={2} opacity={0.55}/>
		<rect x={x + 30} y={y - 26} width={title.length * 16.5 + 44} height={52} rx={26} fill={color} opacity={0.9}/>
		<text x={x + 52} y={y + 8} fontFamily="Inter,Arial,sans-serif" fontSize={26} fontWeight={800} letterSpacing={1.5} fill="#0d1117">
			{title}
		</text>
	</g>
);

export const Badge = ({x, y, text, color}: {x: number; y: number; text: string; color: string}) => (
	<g>
		<rect x={x - text.length * 5.9} y={y - 15} width={text.length * 11.8} height={30} rx={15} fill="#141a21" stroke={color} strokeWidth={1.4}/>
		<text x={x} y={y + 6} textAnchor="middle" fontFamily="Inter,Arial,sans-serif" fontSize={15} fontWeight={600} letterSpacing={1.2} fill={color}>
			{text}
		</text>
	</g>
);

export const useFrame = useCurrentFrame;
