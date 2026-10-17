PORTNAME=	monopsony-client
PORTVERSION=	1.0.0.rc2
PORTREVISION=	0
CATEGORIES=	net
MASTER_SITES=	https://base.societus.club/societus/monopsony/releases/download/v${PORTVERSION:R}/

MAINTAINER=	packaging@societus.club
COMMENT=	Monopsony client daemon - local HTTP proxy and package manager integration

LICENSE=	MIT
LICENSE_FILE=	${WRKSRC}/LICENSE

USES=		cargo
USE_RC_SUBR=	monopsony_client

CARGO_BUILD=	yes
CARGO_INSTALL=	yes
CARGO_TEST=	no

# Substitute paths in the systemd unit (FreeBSD uses rc.d instead)
SUB_FILES=	pkg-message
SUB_LIST+=	PORTNAME=${PORTNAME}

PLIST_FILES=	bin/${PORTNAME} \
		etc/monopsony/client.toml.example \
		etc/${PORTNAME}.conf \
		man/man1/${PORTNAME}.1.gz

OPTIONS_DEFINE=	DOCS
OPTIONS_SUB=	yes

.include <bsd.port.mk>
