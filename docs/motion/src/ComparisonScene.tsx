import {AbsoluteFill, useCurrentFrame} from 'remotion';
import {Badge, halo, LoopArrow, Node, NodeSpec, Panel, Rail, TravelDot} from './parts';

export type ComparisonSpec = {
	width: number;
	height: number;
	accentA: string; // top rail / failure accent
	accentB: string; // pixel accent (panel tab, loop label)
	dotColor: string;
	panelA: {title: string; nodes: NodeSpec[]; loop: string};
	panelB: {
		title: string;
		leftText: string[];
		setup?: {nodes: NodeSpec[]; badge?: string};
		session: {nodes: NodeSpec[]};
		loop: string;
	};
};

const topY = 150;
const setupY = 520;
const sessY = 780;

const arrivals = (n: number, dur: number) =>
	Array.from({length: n}, (_, i) => (i * dur) / (n - 1));

export const ComparisonScene = ({spec}: {spec: ComparisonSpec}) => {
	const frame = useCurrentFrame();
	const {panelA, panelB} = spec;
	const topXs = panelA.nodes.map((n) => n.x);
	const sessXs = panelB.session.nodes.map((n) => n.x);
	const setupXs = panelB.setup?.nodes.map((n) => n.x) ?? [];

	const topDur = 360;
	const setupDur = setupXs.length ? (setupXs.length - 1) * 80 : 0;
	const sessDur = 360;

	const topArrivals = arrivals(topXs.length, topDur);
	const setupArrivals = setupXs.length ? arrivals(setupXs.length, setupDur) : [];
	const sessArrivals = arrivals(sessXs.length, sessDur);

	const lastSetup = panelB.setup?.nodes[setupXs.length - 1];

	return (
		<AbsoluteFill style={{background: '#0d1117'}}>
			<svg width={spec.width} height={spec.height} viewBox={`0 0 ${spec.width} ${spec.height}`}>
				{/* ── WITHOUT ── */}
				<Panel x={40} y={46} w={spec.width - 80} h={300} title={panelA.title} color="#4a3a36"/>
				<Rail nodes={panelA.nodes} color="#6b4a42"/>
				<g transform={`translate(0, ${topY})`}>
					<TravelDot frame={frame} xs={topXs} duration={topDur} color={spec.accentA}/>
				</g>
				{panelA.nodes.map((n, i) => (
					<Node key={n.label} spec={n} glow={halo(frame, topArrivals[i])}/>
				))}
				<LoopArrow
					x1={topXs[0]}
					x2={topXs[topXs.length - 1]}
					y={330}
					color={spec.accentA}
					label={panelA.loop}
				/>

				{/* ── WITH PIXEL ── */}
				<Panel x={40} y={400} w={spec.width - 80} h={spec.height - 40 - 400} title={panelB.title} color={spec.accentB}/>

				{panelB.leftText.length > 0 && (
					<text x={120} y={540} fontFamily="Inter,Arial,sans-serif" fontSize={34} fontWeight={800} fill="#3d444d" letterSpacing={1}>
						{panelB.leftText.map((line, i) => (
							<tspan key={line} x={120} dy={i === 0 ? 0 : 40}>
								{line}
							</tspan>
						))}
					</text>
				)}

				{panelB.setup && lastSetup && (
					<>
						<Rail nodes={panelB.setup.nodes} color="#2f4a75"/>
						<path
							d={`M ${lastSetup.x + 42} ${setupY} H ${sessXs[0] + 50} V ${sessY - 42}`}
							fill="none"
							stroke="#2f4a75"
							strokeWidth={2.6}
							opacity={0.7}
						/>
						{panelB.setup.badge && (
							<Badge x={sessXs[0] - 20} y={640} text={panelB.setup.badge} color="#8b949e"/>
						)}
						<g transform={`translate(0, ${setupY})`}>
							{frame <= setupDur && (
								<TravelDot frame={frame} xs={setupXs} duration={setupDur} color={spec.dotColor}/>
							)}
						</g>
						{panelB.setup.nodes.map((n, i) => (
							<Node key={n.label} spec={n} glow={halo(frame, setupArrivals[i])}/>
						))}
					</>
				)}

				<Rail nodes={panelB.session.nodes} color="#2f4a75"/>
				<g transform={`translate(0, ${sessY})`}>
					<TravelDot frame={frame} xs={sessXs} duration={sessDur} color={spec.dotColor}/>
				</g>
				{panelB.session.nodes.map((n, i) => (
					<Node key={n.label} spec={n} glow={halo(frame, sessArrivals[i])}/>
				))}

				<LoopArrow
					x1={sessXs[sessXs.length - 1]}
					x2={sessXs[0]}
					y={930}
					color={spec.accentB}
					label={panelB.loop}
				/>
			</svg>
		</AbsoluteFill>
	);
};
